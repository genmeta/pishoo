#![cfg(unix)]

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use clap::Parser;
use gateway::{
    control_plane::{Identity, ListenRequest},
    error::Whatever,
    parse::{Node, Value},
    reverse::MissingRulePolicy,
};
use gm_quic::prelude::{QuicListeners, handy::server_parameters};
use nix::{sys::signal::Signal, unistd::Pid};
use rustls::server::WebPkiClientVerifier;
use snafu::{Report, ResultExt, whatever};
use tokio::fs;
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};

mod signal;

struct RootReloadSnapshot {
    entry_config: pishoo::config::EntryConfig,
    worker_targets: Vec<pishoo::config::ResolvedWorkerTarget>,
    owner_map: HashMap<String, pishoo::config::EntryServerOwner>,
    publish_configs: HashMap<String, gateway::dns::PublishConfig>,
}

struct LocalServiceHandle {
    shutdown: CancellationToken,
    task: AbortOnDropHandle<()>,
}

impl LocalServiceHandle {
    async fn shutdown(self) {
        self.shutdown.cancel();
        let _ = self.task.await;
    }
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Set configuration file
    #[arg(short, default_value = "/etc/pishoo/pishoo.conf")]
    config_file: PathBuf,
    /// Send signal to a master process (only on Linux/MacOS)
    #[arg(short, default_value = None)]
    signal: Option<SignalType>,
    /// Test configuration and exit
    #[arg(short, default_value_t = false)]
    test_config: bool,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy)]
pub enum SignalType {
    Stop,
    Quit,
    Reopen,
    Reload,
}

#[tokio::main]
#[snafu::report]
async fn main() -> Result<(), Whatever> {
    let args = Args::parse();

    #[cfg(not(feature = "console_subscriber"))]
    let _tracing_guard =
        pishoo::tracing_init::init_tracing(&format!("pishoo/{}", std::process::id()));

    #[cfg(feature = "console_subscriber")]
    console_subscriber::init();

    let config_file = args.config_file;

    let config = fs::read(&config_file).await.whatever_context(format!(
        "failed to read configuration file at `{}`",
        config_file.display()
    ))?;
    let config = gateway::parse::parse(&config, config_file.parent()).whatever_context(format!(
        "failed to parse configuration file at `{}`",
        config_file.display()
    ))?;

    let entry_config = pishoo::config::parse_entry_config(&config)
        .whatever_context("failed to parse pishoo entry configuration")?;

    if args.test_config {
        let summary = pishoo::config::validate_entry_tree(&entry_config)
            .await
            .whatever_context("configuration validation failed")?;
        tracing::info!(
            path = %config_file.display(),
            workers = summary.workers,
            local_servers = summary.local_servers,
            worker_servers = summary.worker_servers,
            "configuration is valid"
        );
        return Ok(());
    }

    let pid_file = entry_config.pid_file.clone();
    let publishable_servers = pishoo::config::discover_entry_servers(&entry_config)
        .await
        .whatever_context("failed to discover servers for dns publishing")?;

    if let Some(signal) = args.signal {
        return signal::send_signal(&pid_file, signal).await;
    }

    let mut current_entry_config = entry_config;
    let mut current_worker_targets =
        pishoo::config::resolve_entry_worker_targets(&current_entry_config)
            .whatever_context("failed to resolve configured worker users")?;
    let mut current_owner_map = pishoo::config::discover_entry_server_owners(&current_entry_config)
        .await
        .whatever_context("failed to discover current server ownership")?;

    // --- Multi-process supervisor setup ---

    // Create QuicListeners (empty — workers add servers via request_listen)
    let roots = pishoo::tls::root_cert_store();
    let tls_client_cert_verifier = WebPkiClientVerifier::builder(roots)
        .allow_unauthenticated()
        .build()
        .expect("failed to build tls client cert verifier");

    let stun_server = std::env::var("STUN_SERVER").unwrap_or_else(|_| "stun.genmeta.net".into());
    let listeners = QuicListeners::builder()
        .with_resolver(Arc::new(gateway::dns::build_query_resolver_chain(
            &publishable_servers,
        )))
        .with_stun(stun_server)
        .with_parameters(server_parameters())
        .with_client_cert_verifier(tls_client_cert_verifier)
        .with_alpns([b"h3".as_slice()])
        .listen(1024)
        .expect("failed to create QuicListeners");

    // Create RootState (interior mutability — no external Mutex needed)
    let state = Arc::new(pishoo::root::state::RootState::new(listeners.clone()));

    // Write PID file (root only)
    signal::init_pid_file(&pid_file).await?;

    let mut local_service_handle = spawn_local_service(&state, &current_entry_config).await?;

    spawn_configured_workers(&state, current_worker_targets.clone()).await?;

    let initial_publish_configs = gateway::dns::build_publish_configs(&publishable_servers)
        .whatever_context("failed to build dns publish configs")?;
    let mut _publisher = spawn_publisher(listeners.clone(), initial_publish_configs).await;

    // Create signal handler once — reused across the main loop so that signals
    // arriving during reload are never lost.
    let mut signals = signal::RootSignalHandler::new()?;

    // Central accept loop: route connections by server_name
    let accept_handle = pishoo::root::network::spawn_accept_loop(state.clone());

    let monitor_handle = pishoo::root::process::spawn_monitor_loop(state.clone());

    // Watch for network interface changes and reconcile bind URIs
    let network_watch_handle = pishoo::root::network::spawn_network_watch_loop(state.clone());

    loop {
        let sig = signals.wait().await;

        match sig {
            signal::RootSignal::SigTerm
            | signal::RootSignal::SigInt
            | signal::RootSignal::SigQuit => {
                let forwarded = match sig {
                    signal::RootSignal::SigTerm => Signal::SIGTERM,
                    signal::RootSignal::SigInt => Signal::SIGINT,
                    signal::RootSignal::SigQuit => Signal::SIGQUIT,
                    _ => unreachable!("matched shutdown signals only"),
                };
                state.forward_unix_signal(forwarded).await;
                accept_handle.abort();
                tracing::info!(?sig, "forwarded shutdown signal to workers");

                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
                loop {
                    let exited = state.collect_exited_workers().await;
                    for pid in exited {
                        state
                            .cleanup_worker_with_reason(pid, "signal_terminate")
                            .await;
                    }
                    if state.worker_pids().await.is_empty() {
                        break;
                    }
                    if tokio::time::Instant::now() >= deadline {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }

                if !state.worker_pids().await.is_empty() {
                    let force_killed = state.force_kill_workers("shutdown_timeout").await;
                    if !force_killed.is_empty() {
                        tracing::warn!(
                            workers = force_killed.len(),
                            "force-killed lingering workers after shutdown timeout"
                        );

                        let force_kill_deadline =
                            tokio::time::Instant::now() + std::time::Duration::from_secs(2);
                        loop {
                            let exited = state.collect_exited_workers().await;
                            for pid in exited {
                                state
                                    .cleanup_worker_with_reason(pid, "forced_shutdown")
                                    .await;
                            }
                            if state.worker_pids().await.is_empty() {
                                break;
                            }
                            if tokio::time::Instant::now() >= force_kill_deadline {
                                break;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        }
                    }
                }
                break;
            }
            signal::RootSignal::SigHup => {
                let next_snapshot = match load_root_reload_snapshot(&config_file).await {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        tracing::warn!(
                            error = %Report::from_error(&error),
                            path = %config_file.display(),
                            "reload preflight failed; keeping current root state"
                        );
                        continue;
                    }
                };

                if let Err(error) = pishoo::config::ensure_reload_supported(
                    &current_entry_config,
                    &current_worker_targets,
                    &current_owner_map,
                    &next_snapshot.entry_config,
                    &next_snapshot.worker_targets,
                    &next_snapshot.owner_map,
                ) {
                    tracing::warn!(
                        error = %Report::from_error(&error),
                        "reload rejected because it requires a full restart"
                    );
                    continue;
                }

                if let Err(error) = replace_local_service(
                    &state,
                    &mut local_service_handle,
                    &next_snapshot.entry_config,
                )
                .await
                {
                    tracing::warn!(
                        error = %Report::from_error(&error),
                        "failed to reload root-local servers; keeping previous worker state"
                    );
                    continue;
                }

                // Scrub conflicted names before forwarding reload to workers,
                // so workers can re-register previously-conflicted names.
                state.scrub_conflicts().await;

                state.forward_unix_signal(Signal::SIGHUP).await;
                let publish_names = next_snapshot
                    .publish_configs
                    .keys()
                    .cloned()
                    .collect::<HashSet<_>>();
                wait_for_reload_servers(&listeners, &publish_names).await;
                current_entry_config = next_snapshot.entry_config;
                current_worker_targets = next_snapshot.worker_targets;
                current_owner_map = next_snapshot.owner_map;
                _publisher =
                    spawn_publisher(listeners.clone(), next_snapshot.publish_configs).await;
                tracing::info!("reload applied to root-local state and forwarded to workers");
            }
            signal::RootSignal::SigUsr1 => {
                reopen_root_log();
                state.forward_unix_signal(Signal::SIGUSR1).await;
                tracing::info!("root log reopened, forwarded reopen signal to workers");
            }
        }
    }

    _publisher = None;
    accept_handle.abort();
    let _ = accept_handle.await;
    monitor_handle.abort();
    let _ = monitor_handle.await;
    network_watch_handle.abort();
    let _ = network_watch_handle.await;
    if let Some(handle) = local_service_handle.take() {
        handle.shutdown().await;
    }
    for pid in state.worker_pids().await {
        state.cleanup_worker_with_reason(pid, "root_shutdown").await;
    }
    _ = fs::remove_file(&pid_file).await;
    Ok(())
}

const ROOT_LOG_DIR: &str = "/var/log/pishoo";

fn reopen_root_log() {
    use std::fs::OpenOptions;

    let log_dir = Path::new(ROOT_LOG_DIR);
    if let Err(error) = std::fs::create_dir_all(log_dir) {
        tracing::warn!(error = %Report::from_error(&error), dir = %log_dir.display(), "failed to create root log directory");
        return;
    }
    let log_file = log_dir.join("root.log");
    let file = match OpenOptions::new().create(true).append(true).open(&log_file) {
        Ok(f) => f,
        Err(error) => {
            tracing::warn!(error = %Report::from_error(&error), path = %log_file.display(), "failed to open root log file");
            return;
        }
    };
    if let Err(error) = nix::unistd::dup2_stderr(&file) {
        tracing::warn!(
            error = %Report::from_error(&error),
            "failed to dup2 stderr for root log reopen"
        );
    }
}

async fn build_local_service_config(
    local_servers: &[Arc<Node>],
) -> Result<pishoo::service::ServiceConfig, Whatever> {
    let canonicalized = pishoo::naming::canonicalize_server_nodes(local_servers)
        .whatever_context("failed to canonicalize local server nodes")?;

    // Collect the first explicit access_rules URI found across local servers.
    let mut access_rules_uri: Option<String> = None;

    let mut server_configs = Vec::new();
    for server in &canonicalized {
        let Some(Value::Listen(listens)) = server.get("listen") else {
            whatever!("local server missing `listen`");
        };
        let listens = listens.clone();
        let Some(Value::ServerName(server_names)) = server.get("server_name") else {
            whatever!("local server missing `server_name`");
        };
        let server_names = server_names.clone();
        let Some(Value::Path(cert_path)) = server.get("ssl_certificate") else {
            whatever!("local server missing `ssl_certificate`");
        };
        let cert_path = cert_path.clone();
        let Some(Value::Path(key_path)) = server.get("ssl_certificate_key") else {
            whatever!("local server missing `ssl_certificate_key`");
        };
        let key_path = key_path.clone();

        if access_rules_uri.is_none()
            && let Some(Value::String(uri)) = server.get("access_rules")
        {
            access_rules_uri = Some(uri.clone());
        }

        let cert_pem = tokio::fs::read(&cert_path).await.whatever_context(format!(
            "failed to read local certificate file `{}`",
            cert_path.display()
        ))?;
        let key_pem = tokio::fs::read(&key_path).await.whatever_context(format!(
            "failed to read local private key file `{}`",
            key_path.display()
        ))?;
        let (certs, key) = pishoo::tls::validate_tls_material(&cert_pem, &key_pem)
            .whatever_context("invalid local tls material")?;

        for configured_name in server_names {
            let name = genmeta_home::identity::Name::try_from_str(configured_name.name.clone())
                .whatever_context(format!("invalid server name `{}`", configured_name.name))?;
            server_configs.push(pishoo::service::ServerConfig {
                listen_request: ListenRequest {
                    identity: Identity::new(name, certs.clone(), key.clone_key()),
                    bind: listens.clone(),
                },
                server_node: server.clone(),
            });
        }
    }

    let local_policy = pishoo::policy::load_policy_bundle(access_rules_uri.as_deref())
        .await
        .whatever_context("failed to load root-local access rules")?;
    let access_rules = local_policy.location_rules;

    Ok(pishoo::service::ServiceConfig {
        servers: server_configs,
        h3_settings: Arc::new(h3x::dhttp::settings::Settings::default()),
        access_rules,
        missing_rule_policy: MissingRulePolicy::Deny,
    })
}

async fn spawn_local_service(
    state: &Arc<pishoo::root::state::RootState>,
    entry_config: &pishoo::config::EntryConfig,
) -> Result<Option<LocalServiceHandle>, Whatever> {
    if entry_config.local_servers.is_empty() {
        return Ok(None);
    }

    let config = build_local_service_config(&entry_config.local_servers).await?;

    let plane = Arc::new(pishoo::root::local_plane::LocalControlPlane::new(
        state.clone(),
    ));
    let shutdown = CancellationToken::new();
    let service_shutdown = shutdown.clone();

    let handle = AbortOnDropHandle::new(tokio::spawn(async move {
        if let Err(error) = pishoo::service::run_service(plane, &config, service_shutdown).await {
            tracing::error!(
                error = %Report::from_error(error.as_ref()),
                "local service exited with error"
            );
        }
    }));

    Ok(Some(LocalServiceHandle {
        shutdown,
        task: handle,
    }))
}

async fn replace_local_service(
    state: &Arc<pishoo::root::state::RootState>,
    handle: &mut Option<LocalServiceHandle>,
    entry_config: &pishoo::config::EntryConfig,
) -> Result<(), Whatever> {
    if let Some(old_handle) = handle.take() {
        old_handle.shutdown().await;
    }

    *handle = spawn_local_service(state, entry_config).await?;
    Ok(())
}

async fn spawn_publisher(
    listeners: Arc<QuicListeners>,
    publish_configs: HashMap<String, gateway::dns::PublishConfig>,
) -> Option<gateway::dns::Publisher> {
    if publish_configs.is_empty() {
        return None;
    }

    tracing::info!(
        servers = publish_configs.len(),
        "starting dns publisher for pishoo"
    );
    gateway::dns::publish_now(&listeners, &publish_configs).await;
    Some(gateway::dns::Publisher::spawn(listeners, publish_configs))
}

async fn load_root_reload_snapshot(config_file: &Path) -> Result<RootReloadSnapshot, Whatever> {
    let config = fs::read(config_file).await.whatever_context(format!(
        "failed to read configuration file at `{}`",
        config_file.display()
    ))?;
    let config = gateway::parse::parse(&config, config_file.parent()).whatever_context(format!(
        "failed to parse configuration file at `{}`",
        config_file.display()
    ))?;
    let entry_config = pishoo::config::parse_entry_config(&config)
        .whatever_context("failed to parse pishoo entry configuration")?;
    let _ = pishoo::config::validate_entry_tree(&entry_config)
        .await
        .whatever_context("configuration validation failed during reload")?;
    let worker_targets = pishoo::config::resolve_entry_worker_targets(&entry_config)
        .whatever_context("failed to resolve configured worker users during reload")?;
    let owner_map = pishoo::config::discover_entry_server_owners(&entry_config)
        .await
        .whatever_context("failed to discover server ownership during reload")?;
    let publishable_servers = pishoo::config::discover_entry_servers(&entry_config)
        .await
        .whatever_context("failed to discover servers for dns publishing during reload")?;
    let publish_configs = gateway::dns::build_publish_configs(&publishable_servers)
        .whatever_context("failed to build dns publish configs during reload")?;

    Ok(RootReloadSnapshot {
        entry_config,
        worker_targets,
        owner_map,
        publish_configs,
    })
}

async fn wait_for_reload_servers(listeners: &Arc<QuicListeners>, publish_names: &HashSet<String>) {
    if publish_names.is_empty() {
        return;
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let active = listeners.servers().into_iter().collect::<HashSet<_>>();
        let missing = publish_names
            .iter()
            .filter(|name| !active.contains(name.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        if missing.is_empty() {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(missing = ?missing, "timed out waiting for listeners to re-register before dns republish");
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Resolve the path of the `pishoo-worker` binary.
///
/// Search order:
/// 1. Runtime env var `PISHOO_WORKER_BIN`
/// 2. Compile-time env var `PISHOO_WORKER_BIN` (set by deb builds)
/// 3. `<exe_dir>/../libexec/pishoo-worker` (Homebrew layout)
/// 4. `<exe_dir>/pishoo-worker` (debug / same-dir fallback)
fn worker_binary_path() -> std::path::PathBuf {
    // 1. Runtime environment variable
    if let Ok(path) = std::env::var("PISHOO_WORKER_BIN") {
        return std::path::PathBuf::from(path);
    }

    // 2. Compile-time environment variable (set during release deb builds)
    if let Some(path) = option_env!("PISHOO_WORKER_BIN") {
        return std::path::PathBuf::from(path);
    }

    if let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    {
        // 3. Homebrew libexec layout: <prefix>/bin/pishoo → <prefix>/libexec/pishoo-worker
        let libexec = exe_dir.join("../libexec/pishoo-worker");
        if libexec.exists() {
            return libexec;
        }

        // 4. Same directory (debug builds, Windows, cargo build output)
        return exe_dir.join("pishoo-worker");
    }

    std::path::PathBuf::from("pishoo-worker")
}

async fn spawn_configured_workers(
    state: &Arc<pishoo::root::state::RootState>,
    worker_targets: Vec<pishoo::config::ResolvedWorkerTarget>,
) -> Result<(), Whatever> {
    if worker_targets.is_empty() {
        return Ok(());
    }

    let worker_bin = worker_binary_path();

    for target in worker_targets {
        let spawned = pishoo::root::process::spawn_worker(
            &worker_bin,
            target.uid,
            target.gid,
            target.username.clone(),
            target.home.clone(),
            state.clone(),
        )
        .await
        .whatever_context(format!(
            "failed to spawn worker for user `{}`",
            target.username
        ))?;
        let pid = spawned.handle.pid();

        if let Err(error) = ensure_worker_identity(&target, pid, &spawned.hello) {
            state.cleanup_worker_tasks(pid).await;
            return Err(error);
        }

        state.register_worker(pid, target.uid, spawned.handle).await;
    }

    Ok(())
}

fn ensure_worker_identity(
    target: &pishoo::config::ResolvedWorkerTarget,
    pid: Pid,
    hello: &pishoo::ipc::WorkerHello,
) -> Result<(), Whatever> {
    let raw_pid = pid.as_raw() as u32;
    if hello.pid != raw_pid
        || hello.uid != target.uid.as_raw()
        || hello.euid != target.uid.as_raw()
        || hello.gid != target.gid.as_raw()
        || hello.egid != target.gid.as_raw()
    {
        snafu::whatever!(
            "worker identity mismatch for user `{}`: pid={} hello_pid={} uid/euid={}/{} expected_uid={} gid/egid={}/{} expected_gid={}",
            target.username,
            pid,
            hello.pid,
            hello.uid,
            hello.euid,
            target.uid,
            hello.gid,
            hello.egid,
            target.gid
        );
    }

    tracing::info!(
        %pid,
        uid = target.uid.as_raw(),
        gid = target.gid.as_raw(),
        user = %target.username,
        "worker privilege separation verified"
    );

    Ok(())
}
