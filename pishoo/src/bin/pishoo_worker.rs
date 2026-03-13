//! pishoo-worker: per-user worker process.
//!
//! Spawned by the root pishoo process with stdin/stdout piped for remoc IPC.
//! Receives [`WorkerBootstrap`] from the root, scans `~/.genmeta` identities,
//! requests listeners from root for each identity, and enters a signal-wait loop.
//!
//! **stdout is reserved for remoc transport** — all logging goes to stderr.

use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;

use firewall_db::service::{domain_service::DomainService, location_service::LocationService};
use gateway::parse::{Node, Value};
use genmeta_home::GenmetaHome;
use h3x::{
    dhttp::settings::Settings,
    remoc::quic::{ListenClient, RemoteConnectClient},
};
use h3x::remoc::quic::connect::serve_quic_connector;
use h3x::quic::Listen;
use pishoo::protocol::{
    ReleaseListen, RequestListen, RootTransportApi, WorkerBootstrap, WorkerHello,
};
use rustls_pemfile::{certs, private_key};
use snafu::Snafu;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

const MAX_CERT_PEM_BYTES: usize = 512 * 1024;
const MAX_KEY_PEM_BYTES: usize = 64 * 1024;

struct ServerRuntime {
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

struct ConnectorRuntime {
    _connector: RemoteConnectClient,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Clone)]
struct WorkerRouting {
    router: Arc<HashMap<String, Arc<Node>>>,
    binds: Arc<HashMap<String, Vec<String>>>,
}

#[derive(Clone)]
struct WorkerPolicy {
    access_rules: Arc<firewall_db::base::matcher::LocationRulesMatcher>,
}

#[derive(Debug, Snafu)]
enum WorkerError {
    #[snafu(display("access_rules not configured for worker"))]
    MissingAccessRules,
    #[snafu(display("invalid access_rules database uri `{message}`"))]
    InvalidAccessRules { message: String },
    #[snafu(display("failed to connect access_rules database: {message}"))]
    AccessRulesDb { message: String },
    #[snafu(display("failed to load location rules: {message}"))]
    AccessRulesLoad { message: String },
    #[snafu(display("failed to read worker policy config `{path}`: {source}"))]
    ReadPolicy { path: String, source: std::io::Error },
    #[snafu(display("failed to read cert `{path}`: {source}"))]
    ReadCert { path: String, source: std::io::Error },
    #[snafu(display("failed to read key `{path}`: {source}"))]
    ReadKey { path: String, source: std::io::Error },
    #[snafu(display("certificate file too large `{path}` ({actual} > {limit})"))]
    CertTooLarge { path: String, actual: usize, limit: usize },
    #[snafu(display("private key file too large `{path}` ({actual} > {limit})"))]
    KeyTooLarge { path: String, actual: usize, limit: usize },
    #[snafu(display("failed to parse certificate PEM `{path}`: {source}"))]
    ParseCert { path: String, source: std::io::Error },
    #[snafu(display("certificate PEM contains no certificate `{path}`"))]
    EmptyCert { path: String },
    #[snafu(display("failed to parse private key PEM `{path}`: {source}"))]
    ParseKey { path: String, source: std::io::Error },
    #[snafu(display("private key PEM contains no key `{path}`"))]
    EmptyKey { path: String },
}

impl ServerRuntime {
    fn stop(self) {
        self.cancel.cancel();
        self.task.abort();
    }
}

impl ConnectorRuntime {
    fn stop(self) {
        self.cancel.cancel();
        self.task.abort();
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
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
    ) = remoc::Connect::io(remoc::Cfg::default(), stdin, stdout).await?;
    tokio::spawn(conn);

    tracing::debug!("remoc connection established, waiting for bootstrap");

    // Receive bootstrap payload from root.
    let bootstrap = base_rx
        .recv()
        .await
        .map_err(|e| format!("failed to receive WorkerBootstrap: {e}"))?
        .ok_or("root closed base channel without sending WorkerBootstrap")?;

    tracing::info!(
        uid = bootstrap.uid,
        username = %bootstrap.username,
        home = %bootstrap.home.display(),
        log_dir = %bootstrap.log_dir.display(),
        "worker bootstrap received"
    );

    reopen_worker_log(&bootstrap.log_dir)?;
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
        .map_err(|e| format!("failed to send startup hello: {e}"))?;

    tracing::info!("startup hello sent to root, worker ready");

    let root_api = bootstrap.root_api;

    // --- Identity scan: discover ~/.genmeta identities and request listeners ---
    let genmeta_home = GenmetaHome::new(bootstrap.home.join(".genmeta"));
    let listeners: Arc<Mutex<HashMap<String, ServerRuntime>>> = Arc::new(Mutex::new(HashMap::new()));
    let h3_settings = Arc::new(Settings::default());

    let worker_policy = load_worker_policy(&bootstrap.home.join(".genmeta/pishoo.conf")).await?;
    let connector_runtime = start_connector_runtime().await?;
    tracing::info!("worker-owned connector runtime started");

    let identities = genmeta_home.identities();
    match identities.list().await {
        Ok(names) => {
            let routing = build_worker_routing(&identities, &names).await;
            let total = names.len();
            for name in &names {
                let identity_dir = identities.join_name(name.borrow());
                let cert_path = identity_dir.join("fullchain.crt");
                let key_path = identity_dir.join("privkey.pem");
                let (cert_pem, key_pem) = read_tls_material(&cert_path, &key_path).await?;
                let server_name = name.as_str().to_string();

                let request = RequestListen {
                    server_name: server_name.clone(),
                    bind: routing
                        .binds
                        .get(&server_name)
                        .cloned()
                        .unwrap_or_default(),
                    cert_pem,
                    key_pem,
                };
                if request.bind.is_empty() {
                    tracing::warn!(%server_name, "skip listener request: no resolved bind URIs");
                    continue;
                }

                match root_api.request_listen(request).await {
                    Ok(listener) => {
                        let runtime = start_server_runtime(
                            listener,
                            server_name.clone(),
                            h3_settings.clone(),
                            routing.router.clone(),
                            worker_policy.clone(),
                        );
                        tracing::info!(%server_name, "acquired listener");
                        listeners.lock().await.insert(server_name, runtime);
                    }
                    Err(e) => {
                        tracing::warn!(%server_name, %e, "failed to request listener");
                    }
                }
            }
            tracing::info!(
                scanned = total,
                acquired = listeners.lock().await.len(),
                "identity scan complete"
            );
        }
        Err(e) => {
            tracing::warn!(%e, "failed to list identities, continuing without listeners");
        }
    }

    // Keep root_api and identity state alive for Reload handling.
    let genmeta_home_path = bootstrap.home.join(".genmeta");

    let mut quit_signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::quit())?;
    let mut hup_signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    let mut usr1_signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())?;
    let mut term_signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut int_signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    loop {
        tokio::select! {
            _ = term_signal.recv() => {
                tracing::info!("received SIGTERM; shutting down worker");
                break;
            }
            _ = int_signal.recv() => {
                tracing::info!("received SIGINT; shutting down worker");
                break;
            }
            _ = quit_signal.recv() => {
                tracing::info!("received SIGQUIT; shutting down worker");
                break;
            }
            _ = hup_signal.recv() => {
                        tracing::info!("reloading identities");
                        let genmeta_home = GenmetaHome::new(genmeta_home_path.clone());
                        let identities = genmeta_home.identities();
                        match identities.list().await {
                            Ok(names) => {
                                let routing = build_worker_routing(&identities, &names).await;
                                let new_set: HashSet<String> = names
                                    .iter()
                                    .map(|n| n.as_str().to_string())
                                    .collect();
                                let old_set: HashSet<String> = listeners
                                    .lock()
                                    .await
                                    .keys()
                                    .cloned()
                                    .collect();

                                let added: Vec<&String> =
                                    new_set.difference(&old_set).collect();
                                let removed: Vec<&String> =
                                    old_set.difference(&new_set).collect();
                                let kept = old_set.intersection(&new_set).count();

                                // Release listeners for removed identities.
                                for server_name in &removed {
                                    let request = ReleaseListen {
                                        server_name: (*server_name).clone(),
                                    };
                                    match root_api.release_listen(request).await {
                                        Ok(()) => {
                                            tracing::info!(%server_name, "released listener");
                                            if let Some(runtime) = listeners.lock().await.remove(*server_name) {
                                                runtime.stop();
                                            }
                                        }
                                        Err(e) => {
                                            tracing::warn!(%server_name, %e, "failed to release listener");
                                        }
                                    }
                                }

                                // Request listeners for new identities.
                                for server_name in &added {
                                    let name = names
                                        .iter()
                                        .find(|n| n.as_str() == server_name.as_str())
                                        .unwrap();
                                    let identity_dir =
                                        identities.join_name(name.borrow());
                                    let cert_path =
                                        identity_dir.join("fullchain.crt");
                                    let key_path =
                                        identity_dir.join("privkey.pem");
                                    let (cert_pem, key_pem) = read_tls_material(&cert_path, &key_path).await?;

                                    let request = RequestListen {
                                        server_name: (*server_name).clone(),
                                        bind: routing
                                            .binds
                                            .get(*server_name)
                                            .cloned()
                                            .unwrap_or_default(),
                                        cert_pem,
                                        key_pem,
                                    };
                                    if request.bind.is_empty() {
                                        tracing::warn!(%server_name, "skip listener request: no resolved bind URIs");
                                        continue;
                                    }

                                    match root_api.request_listen(request).await {
                                        Ok(listener) => {
                                            let runtime = start_server_runtime(
                                                listener,
                                                (*server_name).clone(),
                                                h3_settings.clone(),
                                                routing.router.clone(),
                                                worker_policy.clone(),
                                            );
                                            tracing::info!(%server_name, "acquired listener");
                                            listeners.lock().await.insert((*server_name).clone(), runtime);
                                        }
                                        Err(e) => {
                                            tracing::warn!(%server_name, %e, "failed to request listener");
                                        }
                                    }
                                }

                                tracing::info!(
                                    added = added.len(),
                                    removed = removed.len(),
                                    kept,
                                    "reload complete"
                                );
                            }
                            Err(e) => {
                                tracing::warn!(%e, "failed to list identities during reload");
                            }
                        }
            }
            _ = usr1_signal.recv() => {
                reopen_worker_log(&bootstrap.log_dir)?;
                tracing::info!("worker log reopened");
            }
        }
    }

    connector_runtime.stop();
    tracing::info!("pishoo-worker exiting");
    Ok(())
}

fn start_server_runtime(
    listener: ListenClient,
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
                            let h3_settings = h3_settings.clone();
                            let server_name = server_name.clone();
                            let router = router.clone();
                            let worker_policy = worker_policy.clone();
                            tokio::spawn(async move {
                                if let Err(e) = gateway::reverse::handle_single_connection_for_worker(
                                    conn,
                                    server_name,
                                    h3_settings,
                                    router,
                                    worker_policy.access_rules,
                                    gateway::reverse::MissingRulePolicy::Deny,
                                ).await {
                                    tracing::warn!(%e, "worker connection handling failed");
                                }
                            });
                        }
                        Err(e) => {
                            tracing::warn!(%e, %server_name, "listener accept failed");
                            break;
                        }
                    }
                }
            }
        }
    });
    ServerRuntime {
        cancel,
        task,
    }
}

async fn start_connector_runtime() -> Result<ConnectorRuntime, WorkerError> {
    use gm_quic::prelude::handy::ToCertificate;

    let mut roots = rustls::RootCertStore::empty();
    roots.add_parsable_certificates(include_bytes!("../../../keychain/root.crt").to_certificate());
    let client = gm_quic::prelude::QuicClient::builder()
        .with_root_certificates(Arc::new(roots))
        .without_cert()
        .with_alpns(vec!["h3"])
        .build();

    let (connector, serve_fut) = serve_quic_connector(Arc::new(client));
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    let task = tokio::spawn(async move {
        tokio::select! {
            () = serve_fut => {}
            () = cancel_clone.cancelled() => {}
        }
    });
    Ok(ConnectorRuntime {
        _connector: connector,
        cancel,
        task,
    })
}

async fn load_worker_policy(conf_path: &Path) -> Result<WorkerPolicy, WorkerError> {
    let raw = tokio::fs::read(conf_path)
        .await
        .map_err(|e| WorkerError::ReadPolicy {
            path: conf_path.display().to_string(),
            source: e,
        })?;
    let parsed = gateway::parse::parse(&raw, conf_path.parent())
        .map_err(|e| WorkerError::InvalidAccessRules { message: e.to_string() })?;
    let pishoo = parsed
        .get("pishoo")
        .and_then(|v| match v { Value::Nodes(nodes) => nodes.first(), _ => None })
        .ok_or(WorkerError::MissingAccessRules)?;
    let uri = match pishoo.get("access_rules") {
        Some(Value::String(uri)) => uri.clone(),
        _ => return Err(WorkerError::MissingAccessRules),
    };

    let db = sea_orm::Database::connect(&uri)
        .await
        .map_err(|e| WorkerError::AccessRulesDb { message: e.to_string() })?;
    let location_rules = LocationService::new(&db)
        .list_all_rules()
        .await
        .map_err(|e| WorkerError::AccessRulesLoad { message: e.to_string() })?;

    let _ = DomainService::new(&db)
        .list_all_rules()
        .await
        .map_err(|e| WorkerError::AccessRulesLoad { message: e.to_string() })?;

    Ok(WorkerPolicy {
        access_rules: Arc::new(location_rules.into()),
    })
}

async fn read_tls_material(cert_path: &Path, key_path: &Path) -> Result<(Vec<u8>, Vec<u8>), WorkerError> {
    let cert_pem = tokio::fs::read(cert_path)
        .await
        .map_err(|e| WorkerError::ReadCert {
            path: cert_path.display().to_string(),
            source: e,
        })?;
    if cert_pem.len() > MAX_CERT_PEM_BYTES {
        return Err(WorkerError::CertTooLarge {
            path: cert_path.display().to_string(),
            actual: cert_pem.len(),
            limit: MAX_CERT_PEM_BYTES,
        });
    }
    let cert_count = certs(&mut Cursor::new(&cert_pem))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| WorkerError::ParseCert {
            path: cert_path.display().to_string(),
            source: e,
        })?
        .len();
    if cert_count == 0 {
        return Err(WorkerError::EmptyCert {
            path: cert_path.display().to_string(),
        });
    }

    let key_pem = tokio::fs::read(key_path)
        .await
        .map_err(|e| WorkerError::ReadKey {
            path: key_path.display().to_string(),
            source: e,
        })?;
    if key_pem.len() > MAX_KEY_PEM_BYTES {
        return Err(WorkerError::KeyTooLarge {
            path: key_path.display().to_string(),
            actual: key_pem.len(),
            limit: MAX_KEY_PEM_BYTES,
        });
    }
    let parsed_key = private_key(&mut Cursor::new(&key_pem)).map_err(|e| WorkerError::ParseKey {
        path: key_path.display().to_string(),
        source: e,
    })?;
    if parsed_key.is_none() {
        return Err(WorkerError::EmptyKey {
            path: key_path.display().to_string(),
        });
    }

    Ok((cert_pem, key_pem))
}

async fn build_worker_routing(
    identities: &genmeta_home::identity::Identities,
    names: &[genmeta_home::identity::Name<'_>],
) -> WorkerRouting {
    let device_names = gm_quic::qinterface::device::Devices::global()
        .interfaces()
        .keys()
        .cloned()
        .collect::<Vec<_>>();

    let mut servers: Vec<Arc<Node>> = Vec::new();
    let mut fallback_entries: HashMap<String, Arc<Node>> = HashMap::new();
    let mut binds: HashMap<String, Vec<String>> = HashMap::new();

    for name in names {
        let identity_dir = identities.join_name(name.borrow());
        let server_name = name.as_str().to_string();
        let fallback = Arc::new(Node::new(Value::ValueMap(HashMap::new())));
        fallback_entries.insert(server_name, fallback);

        let conf_path = identity_dir.join("pishoo.conf");
        if !conf_path.is_file() {
            continue;
        }
        if let Ok(raw) = tokio::fs::read(&conf_path).await
            && let Ok(parsed) = gateway::parse::parse(&raw, conf_path.parent())
            && let Some(Value::Nodes(pishoo_nodes)) = parsed.get("pishoo")
            && let Some(pishoo_node) = pishoo_nodes.first()
            && let Some(Value::Nodes(server_nodes)) = pishoo_node.get("server")
        {
            for server_node in server_nodes {
                if let (Some(Value::ServerName(server_names)), Some(Value::Listen(listens))) =
                    (server_node.get("server_name"), server_node.get("listen"))
                {
                    for server_name in server_names {
                        let normalized = match server_name.name.strip_suffix('~') {
                            Some(prefix) => format!("{prefix}.genmeta.net"),
                            None => server_name.name.clone(),
                        };
                        let bind_set = listens
                            .iter()
                            .flat_map(|listen| {
                                listen.resolve(device_names.iter().map(|s| s.as_str()))
                            })
                            .filter(|uri| uri.resolve().is_ok())
                            .map(|uri| uri.to_string())
                            .collect::<std::collections::HashSet<_>>();
                        binds.insert(normalized, bind_set.into_iter().collect());
                    }
                }
            }
            servers.extend(server_nodes.iter().cloned());
        }
    }

    let mut router = gateway::reverse::build_router_for_worker(&servers)
        .as_ref()
        .clone();
    for (name, node) in fallback_entries {
        router.entry(name).or_insert(node);
    }
    WorkerRouting {
        router: Arc::new(router),
        binds: Arc::new(binds),
    }
}

fn reopen_worker_log(log_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(unix)]
    {
        use std::fs::OpenOptions;
        use std::os::fd::AsRawFd;

        std::fs::create_dir_all(log_dir)?;
        let log_file = log_dir.join("worker.log");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_file)?;
        let ret = unsafe { libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO) };
        if ret == -1 {
            return Err(Box::new(std::io::Error::last_os_error()));
        }
    }
    Ok(())
}
