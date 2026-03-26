//! pishoo-worker: per-user worker process.
//!
//! Spawned by the root pishoo process with stdin/stdout piped for remoc IPC.
//! Receives [`WorkerBootstrap`] from the root, scans `~/.genmeta` identities,
//! requests listeners from root for each identity, and enters a signal-wait loop.
//!
//! **stdout is reserved for remoc transport** — all logging goes to stderr.

use std::{
    collections::{HashMap, HashSet},
    io,
    path::Path,
    sync::Arc,
};

use gateway::{
    error::Whatever,
    parse::{Node, Value},
};
use genmeta_home::{GenmetaHome, identity::Name};
use h3x::{dhttp::settings::Settings, remoc::quic::ConnectionClient};
use pishoo::{
    bind::resolve_bind_uris,
    config::load_identity_servers,
    policy,
    protocol::{
        OpenConnector, ReleaseListen, RequestListen, RootTransportApi, WorkerBootstrap, WorkerHello,
    },
};
use snafu::{OptionExt, Report, ResultExt, Snafu};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

struct ServerRuntime {
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Debug, Clone)]
struct WorkerRouting {
    router: Arc<HashMap<String, Arc<Node>>>,
    // TODO: check
    binds: Arc<HashMap<String, Vec<String>>>,
}

#[derive(Debug, Clone)]
struct WorkerPolicy {
    access_rules: Arc<firewall_db::base::matcher::LocationRulesMatcher>,
}

struct ReconcileStats {
    added: usize,
    removed: usize,
    kept: usize,
}

struct WorkerReloadPlan {
    desired_listeners: Vec<RequestListen>,
    routing: WorkerRouting,
    worker_policy: WorkerPolicy,
    stats: ReconcileStats,
}

#[derive(Debug, Snafu)]
enum WorkerError {
    #[snafu(display("access_rules not configured for worker"))]
    MissingAccessRules,
    #[snafu(display("failed to parse worker policy config `{path}`"))]
    ParsePolicy {
        path: String,
        source: gateway::error::Whatever,
    },
    #[snafu(display("failed to connect access_rules database"))]
    AccessRulesDb { source: policy::PolicyError },
    #[snafu(display("failed to read worker policy config `{path}`"))]
    ReadPolicy {
        path: String,
        source: std::io::Error,
    },
}

impl ServerRuntime {
    fn stop(self) {
        self.cancel.cancel();
        self.task.abort();
    }
}

#[tokio::main(flavor = "current_thread")]
#[snafu::report]
async fn main() -> Result<(), Whatever> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    // Establish remoc connection over stdin (read) / stdout (write).
    //
    // Base channel types (child perspective — mirrored from parent):
    //   Receiver<WorkerBootstrap> — child receives bootstrap from parent
    let (conn, mut base_tx, mut base_rx): (
        _,
        remoc::rch::base::Sender<WorkerHello>,
        remoc::rch::base::Receiver<WorkerBootstrap>,
    ) = remoc::Connect::io(remoc::Cfg::default(), stdin, stdout)
        .await
        .whatever_context("failed to establish remoc transport")?;
    tokio::spawn(conn.in_current_span());

    tracing::debug!("remoc connection established, waiting for bootstrap");

    // Receive bootstrap payload from root.
    let bootstrap = base_rx
        .recv()
        .await
        .whatever_context("failed to receive worker bootstrap")?
        .whatever_context("root closed base channel without sending worker bootstrap")?;

    tracing::info!(
        uid = bootstrap.uid,
        username = %bootstrap.username,
        home = %bootstrap.home.display(),
        "worker bootstrap received"
    );

    // TODO: domain logs
    // reopen_worker_log(&bootstrap.log_dir).whatever_context("failed to reopen worker log")?;
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    tracing::info!("pishoo-worker starting");

    let pid = std::process::id();
    let hello = WorkerHello {
        pid,
        uid: nix::unistd::getuid().as_raw(),
        euid: nix::unistd::geteuid().as_raw(),
        gid: nix::unistd::getgid().as_raw(),
        egid: nix::unistd::getegid().as_raw(),
    };

    base_tx
        .send(hello)
        .await
        .whatever_context("failed to send startup hello")?;

    tracing::info!("startup hello sent to root, worker ready");

    let root_api = bootstrap.root_api;

    // --- Identity scan: discover ~/.genmeta identities and request listeners ---
    let genmeta_home = GenmetaHome::new(bootstrap.home.join(".genmeta"));
    let listeners: Arc<Mutex<HashMap<Name<'static>, ServerRuntime>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let h3_settings = Arc::new(Settings::default());
    let worker_policy_path = bootstrap.home.join(".genmeta/pishoo.conf");

    let worker_policy = load_worker_policy(&worker_policy_path)
        .await
        .whatever_context("failed to load worker policy")?;
    // connector root-owned: worker 不再本地创建 connector runtime.
    let _connector_handle = root_api
        .open_connector(OpenConnector {
            profile: String::new(),
        })
        .await
        .whatever_context("failed to open root-owned connector")?;
    tracing::info!("root-owned connector handle acquired");

    let current_servers = HashSet::new();
    match build_worker_reload_plan(&genmeta_home, current_servers, worker_policy.clone()).await {
        Ok(plan) => {
            match apply_worker_reload_plan(&root_api, &listeners, plan, h3_settings.clone()).await {
                Ok(stats) => {
                    tracing::info!(
                        scanned = stats.added,
                        acquired = listeners.lock().await.len(),
                        "identity scan complete"
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        error = %Report::from_error(&error),
                        "failed to apply initial listener plan, continuing without listeners"
                    );
                }
            }
        }
        Err(error) => {
            tracing::warn!(
                error = %Report::from_error(&error),
                "failed to build initial listener plan, continuing without listeners"
            );
        }
    }

    // Keep root_api and identity state alive for Reload handling.
    let mut quit_signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::quit())
        .whatever_context("failed to create sigquit listener")?;
    let mut hup_signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .whatever_context("failed to create sighup listener")?;
    let mut usr1_signal =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())
            .whatever_context("failed to create sigusr1 listener")?;
    let mut term_signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .whatever_context("failed to create sigterm listener")?;
    let mut int_signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .whatever_context("failed to create sigint listener")?;

    loop {
        tokio::select! {
            _ = term_signal.recv() => {
                tracing::info!("received sigterm; shutting down worker");
                break;
            }
            _ = int_signal.recv() => {
                tracing::info!("received sigint; shutting down worker");
                break;
            }
            _ = quit_signal.recv() => {
                tracing::info!("received sigquit; shutting down worker");
                break;
            }
            _ = hup_signal.recv() => {
                tracing::info!("reloading identities");
                let current_servers = listeners.lock().await.keys().cloned().collect();
                let worker_policy = match load_worker_policy(&worker_policy_path).await {
                    Ok(worker_policy) => worker_policy,
                    Err(error) => {
                        tracing::warn!(
                            error = %Report::from_error(&error),
                            "worker policy reload failed; keeping current listeners"
                        );
                        continue;
                    }
                };

                let plan = match build_worker_reload_plan(
                    &genmeta_home,
                    current_servers,
                    worker_policy,
                ).await {
                    Ok(plan) => plan,
                    Err(error) => {
                        tracing::warn!(
                            error = %Report::from_error(&error),
                            "failed to build reload plan; keeping current listeners"
                        );
                        continue;
                    }
                };

                match apply_worker_reload_plan(&root_api, &listeners, plan, h3_settings.clone()).await {
                    Ok(stats) => {
                        tracing::info!(
                            added = stats.added,
                            removed = stats.removed,
                            kept = stats.kept,
                            "reload complete"
                        );
                    }
                    Err(error) => {
                        tracing::warn!(
                            error = %Report::from_error(&error),
                            "failed to apply reload plan"
                        );
                    }
                }
            }
            _ = usr1_signal.recv() => {
                // reopen_worker_log(&bootstrap.log_dir)
                //     .whatever_context("failed to reopen worker log")?;
                tracing::info!("worker log reopened");
            }
        }
    }
    stop_server_runtimes(&listeners).await;
    tracing::info!("pishoo-worker exiting");
    Ok(())
}

async fn stop_server_runtimes(listeners: &Arc<Mutex<HashMap<Name<'static>, ServerRuntime>>>) {
    let runtimes = {
        let mut listeners = listeners.lock().await;
        listeners
            .drain()
            .map(|(_, runtime)| runtime)
            .collect::<Vec<_>>()
    };

    for runtime in runtimes {
        runtime.stop();
    }
}

fn start_server_runtime(
    listener: pishoo::remoc_bridge::ListenerHandle,
    server_name: String,
    h3_settings: Arc<Settings>,
    router: Arc<HashMap<String, Arc<Node>>>,
    worker_policy: WorkerPolicy,
) -> ServerRuntime {
    let cancel = CancellationToken::new();
    let cancel_child = cancel.clone();
    let task = tokio::spawn(async move {
        let listener_for_task = listener;
        loop {
            tokio::select! {
                () = cancel_child.cancelled() => {
                    break;
                }
                accepted = listener_for_task.accept() => {
                    match accepted {
                        Ok(conn) => {
                            let conn: ConnectionClient = conn;
                            let h3_settings = h3_settings.clone();
                            let server_name = server_name.clone();
                            let router = router.clone();
                            let worker_policy = worker_policy.clone();
                        tokio::spawn(async move {
                                if let Err(error) = gateway::reverse::handle_single_connection_for_worker(
                                    conn,
                                    server_name,
                                    h3_settings,
                                    router,
                                    worker_policy.access_rules,
                                    gateway::reverse::MissingRulePolicy::Deny,
                                ).await {
                                    tracing::warn!(error = %Report::from_error(&error), "worker connection handling failed");
                                }
                            }
                            .in_current_span());
                        }
                        Err(error) => {
                            tracing::warn!(error = %Report::from_error(&error), %server_name, "listener accept failed");
                            break;
                        }
                    }
                }
            }
        }
    }
    .in_current_span());
    ServerRuntime { cancel, task }
}

async fn build_worker_reload_plan(
    genmeta_home: &GenmetaHome,
    current_servers: HashSet<Name<'static>>,
    worker_policy: WorkerPolicy,
) -> Result<WorkerReloadPlan, Whatever> {
    let names: HashSet<Name<'static>> = genmeta_home
        .identities()
        .list()
        .await
        .whatever_context("failed to list identities")?
        .into_iter()
        .collect();
    let routing = build_worker_routing(genmeta_home, &names).await?;
    let mut desired_servers = HashSet::new();
    let mut desired_listeners = Vec::new();

    for server_name in &names {
        let identity = genmeta_home
            .identities()
            .load(server_name.borrow())
            .await
            .whatever_context(format!(
                "failed to read tls material for identity `{server_name}`"
            ))?;

        let request = RequestListen {
            name: server_name.clone(),
            bind: routing
                .binds
                .get(server_name.as_full())
                .cloned()
                .unwrap_or_default(),
            certs: identity.certs().to_vec(),
            key: identity.key().clone_key(),
        };
        if request.bind.is_empty() {
            tracing::warn!(%server_name, "skip listener request: no resolved bind uris");
            continue;
        }

        desired_servers.insert(server_name.clone());
        desired_listeners.push(request);
    }

    let added = desired_servers.difference(&current_servers).count();
    let removed = current_servers.difference(&desired_servers).count();
    let kept = current_servers.intersection(&desired_servers).count();

    desired_listeners.sort_by(|left, right| left.name.as_full().cmp(right.name.as_full()));

    Ok(WorkerReloadPlan {
        desired_listeners,
        routing,
        worker_policy,
        stats: ReconcileStats {
            added,
            removed,
            kept,
        },
    })
}

async fn apply_worker_reload_plan(
    root_api: &impl RootTransportApi,
    listeners: &Arc<Mutex<HashMap<Name<'static>, ServerRuntime>>>,
    plan: WorkerReloadPlan,
    h3_settings: Arc<Settings>,
) -> Result<ReconcileStats, Whatever> {
    let WorkerReloadPlan {
        desired_listeners,
        routing,
        worker_policy,
        stats,
    } = plan;

    let desired_by_name = desired_listeners
        .into_iter()
        .map(|request| (request.name.clone(), request))
        .collect::<HashMap<_, _>>();

    let current_runtimes = {
        let mut listeners = listeners.lock().await;
        listeners.drain().collect::<Vec<_>>()
    };

    let mut next_runtimes = HashMap::new();

    for (server_name, runtime) in current_runtimes {
        root_api
            .release_listen(ReleaseListen {
                server_name: server_name.clone(),
            })
            .await
            .whatever_context(format!(
                "failed to release listener `{server_name}` during reload"
            ))?;
        tracing::info!(%server_name, "released listener");
        runtime.stop();
    }

    for (server_name, request) in desired_by_name {
        let listener = root_api
            .request_listen(request)
            .await
            .whatever_context(format!(
                "failed to request listener `{server_name}` during reload"
            ))?;
        let runtime = start_server_runtime(
            listener,
            server_name.as_full().to_owned(),
            h3_settings.clone(),
            routing.router.clone(),
            worker_policy.clone(),
        );
        tracing::info!(%server_name, "acquired listener");
        next_runtimes.insert(server_name, runtime);
    }

    let mut listeners = listeners.lock().await;
    *listeners = next_runtimes;

    Ok(stats)
}

async fn load_worker_policy(conf_path: &Path) -> Result<WorkerPolicy, WorkerError> {
    let path = conf_path.display().to_string();
    let raw = tokio::fs::read(conf_path)
        .await
        .context(ReadPolicySnafu { path: &path })?;
    let parsed = gateway::parse::parse(&raw, conf_path.parent())
        .context(ParsePolicySnafu { path: &path })?;
    let pishoo = parsed
        .get("pishoo")
        .and_then(|v| match v {
            Value::Nodes(nodes) => nodes.first(),
            _ => None,
        })
        .ok_or(WorkerError::MissingAccessRules)?;
    let uri = match pishoo.get("access_rules") {
        Some(Value::String(uri)) => uri.clone(),
        _ => return Err(WorkerError::MissingAccessRules),
    };

    let bundle = policy::load_policy_bundle(Some(uri.as_str()))
        .await
        .context(AccessRulesDbSnafu)?;

    Ok(WorkerPolicy {
        access_rules: bundle.location_rules,
    })
}

async fn build_worker_routing(
    genemta_home: &GenmetaHome,
    names: &HashSet<Name<'_>>,
) -> Result<WorkerRouting, Whatever> {
    let device_names = gm_quic::qinterface::device::Devices::global()
        .interfaces()
        .keys()
        .cloned()
        .collect::<Vec<_>>();

    let mut servers: Vec<Arc<Node>> = Vec::new();
    let mut fallback_entries: HashMap<String, Arc<Node>> = HashMap::new();
    let mut binds: HashMap<String, Vec<String>> = HashMap::new();

    for name in names {
        let server_name = name.as_full().to_string();
        let fallback = Arc::new(Node::new(Value::ValueMap(HashMap::new())));
        fallback_entries.insert(server_name, fallback);
        let identity_dir = genemta_home.identities().join_name(name.borrow());

        let conf_path = identity_dir.join("pishoo.conf");
        if !conf_path.is_file() {
            continue;
        }
        let canonicalized_server_nodes =
            load_identity_servers(&conf_path)
                .await
                .whatever_context(format!(
                    "failed to load identity servers from `{}` for `{}`",
                    conf_path.display(),
                    name.as_full()
                ))?;

        for server_node in &canonicalized_server_nodes {
            if let (Some(Value::ServerName(server_names)), Some(Value::Listen(listens))) =
                (server_node.get("server_name"), server_node.get("listen"))
            {
                for server_name in server_names {
                    binds.insert(
                        server_name.name.clone(),
                        resolve_bind_uris(listens, &device_names),
                    );
                }
            }
        }
        servers.extend(canonicalized_server_nodes);
    }

    let mut router = gateway::reverse::build_router_for_servers(&servers)
        .as_ref()
        .clone();
    for (name, node) in fallback_entries {
        router.entry(name).or_insert(node);
    }
    Ok(WorkerRouting {
        router: Arc::new(router),
        binds: Arc::new(binds),
    })
}

#[allow(dead_code)]
fn validate_identity_tls_material(
    cert_pem: &[u8],
    key_pem: &[u8],
) -> Result<(), pishoo::tls::TlsMaterialError> {
    let _ = pishoo::tls::validate_tls_material(cert_pem, key_pem)?;
    Ok(())
}

#[allow(dead_code)]
#[derive(Debug, Snafu)]
enum ReopenWorkerLogError {
    #[snafu(display("failed to create worker log directory `{path}`"))]
    CreateLogDir { path: String, source: io::Error },
    #[snafu(display("failed to open worker log file `{path}`"))]
    OpenLogFile { path: String, source: io::Error },
    #[snafu(display("failed to dup2 stderr for worker log reopen"))]
    DupStderr { source: nix::errno::Errno },
}

#[allow(dead_code)]
fn reopen_worker_log(log_dir: &Path) -> Result<(), ReopenWorkerLogError> {
    use std::fs::OpenOptions;

    std::fs::create_dir_all(log_dir).context(CreateLogDirSnafu {
        path: log_dir.display().to_string(),
    })?;
    let log_file = log_dir.join("worker.log");
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_file)
        .context(OpenLogFileSnafu {
            path: log_file.display().to_string(),
        })?;
    nix::unistd::dup2_stderr(&file).context(DupStderrSnafu)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("pishoo crate should live under repo root")
            .to_path_buf()
    }

    fn repo_rules_db_uri() -> String {
        format!(
            "sqlite://{}?mode=rw",
            repo_root().join("rules.db").display()
        )
    }

    fn repo_tls_paths() -> (PathBuf, PathBuf) {
        let base = repo_root().join("keychain/borber.pilot.genmeta.net");
        (
            base.join("borber.pilot.genmeta.net.pem"),
            base.join("borber.pilot.genmeta.net.key"),
        )
    }

    fn temp_home() -> PathBuf {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let home = std::env::temp_dir().join(format!(
            "pishoo-worker-reload-{}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock should be after epoch")
                .as_nanos(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir_all(&home).expect("create temp home");
        home
    }

    fn write_worker_layout(home: &std::path::Path, identity_dir_name: &str, server_name: &str) {
        let (cert, key) = repo_tls_paths();
        let genmeta_dir = home.join(".genmeta");
        let identity_dir = genmeta_dir.join("identity").join(identity_dir_name);
        std::fs::create_dir_all(&identity_dir).expect("create identity dir");
        std::fs::write(
            genmeta_dir.join("pishoo.conf"),
            format!("pishoo {{ access_rules {}; }}", repo_rules_db_uri()),
        )
        .expect("write worker policy");
        std::fs::write(
            identity_dir.join("pishoo.conf"),
            format!(
                "pishoo {{ server {{ listen all 443; server_name {server_name}; ssl_certificate {}; ssl_certificate_key {}; location / {{ root {}; }} }} }}",
                cert.display(),
                key.display(),
                home.display(),
            ),
        )
        .expect("write identity config");
        std::fs::copy(&cert, identity_dir.join("fullchain.crt")).expect("copy identity cert");
        let key_path = identity_dir.join("privkey.pem");
        std::fs::copy(&key, &key_path).expect("copy identity key");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o400))
                .expect("tighten identity key permissions");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn build_worker_reload_plan_keeps_existing_identity_in_rebuild_set() {
        let home = temp_home();
        write_worker_layout(&home, "borber.pilot", "borber.pilot.genmeta.net");
        let genmeta_home = GenmetaHome::new(home.join(".genmeta"));
        let worker_policy = load_worker_policy(&home.join(".genmeta/pishoo.conf"))
            .await
            .expect("load worker policy");
        let current_servers = HashSet::from([Name::try_from_str_partial("borber.pilot")
            .expect("valid identity name")
            .into_owned()]);

        let plan = build_worker_reload_plan(&genmeta_home, current_servers, worker_policy)
            .await
            .expect("build reload plan");

        assert_eq!(plan.stats.added, 0);
        assert_eq!(plan.stats.removed, 0);
        assert_eq!(plan.stats.kept, 1);
        assert_eq!(plan.desired_listeners.len(), 1);
        assert_eq!(
            plan.desired_listeners[0].name.as_full(),
            "borber.pilot.genmeta.net"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn build_worker_routing_surfaces_invalid_identity_config() {
        let home = temp_home();
        let genmeta_dir = home.join(".genmeta");
        let identity_dir = genmeta_dir.join("identity/borber.pilot");
        std::fs::create_dir_all(&identity_dir).expect("create identity dir");
        std::fs::write(
            genmeta_dir.join("pishoo.conf"),
            format!("pishoo {{ access_rules {}; }}", repo_rules_db_uri()),
        )
        .expect("write worker policy");
        std::fs::write(
            identity_dir.join("pishoo.conf"),
            b"server { listen all 443; }",
        )
        .expect("write invalid identity config");

        let genmeta_home = GenmetaHome::new(genmeta_dir);
        let names = HashSet::from([Name::try_from_str_partial("borber.pilot")
            .expect("valid identity name")
            .into_owned()]);

        let err = build_worker_routing(&genmeta_home, &names)
            .await
            .expect_err("invalid identity config should fail reload plan build");

        assert!(err.to_string().contains("failed to load identity servers"));
    }
}
