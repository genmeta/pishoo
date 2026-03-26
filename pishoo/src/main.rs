#![cfg(unix)]

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use clap::Parser;
use gateway::error::Whatever;
use gm_quic::prelude::{QuicListeners, handy::server_parameters};
use nix::{sys::signal::Signal, unistd::Pid};
use rustls::server::WebPkiClientVerifier;
use snafu::{Report, ResultExt};
use tokio::fs;
use tracing::Instrument;

mod signal;

struct RootReloadSnapshot {
    entry_config: pishoo::config::EntryConfig,
    worker_targets: Vec<pishoo::config::ResolvedWorkerTarget>,
    owner_map: HashMap<String, pishoo::config::EntryServerOwner>,
    publish_configs: HashMap<String, gateway::dns::PublishConfig>,
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
    {
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::builder()
                    .with_default_directive(tracing::Level::DEBUG.into())
                    .from_env_lossy(),
            )
            .with_ansi(atty::is(atty::Stream::Stdout));
        #[cfg(debug_assertions)]
        let subscriber = subscriber.with_file(true).with_line_number(true);
        subscriber.init();
    }

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
            shape = ?summary.shape,
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

    let listeners = QuicListeners::builder()
        .with_resolver(Arc::new(gateway::dns::build_query_resolver_chain(
            &publishable_servers,
        )))
        .with_stun("stun.genmeta.net")
        .with_parameters(server_parameters())
        .with_client_cert_verifier(tls_client_cert_verifier)
        .with_alpns([b"h3".as_slice()])
        .listen(1024)
        .expect("failed to create QuicListeners");

    // Create QuicClient for outbound connectors
    let root_store = pishoo::tls::root_cert_store();
    let quic_client = gm_quic::prelude::QuicClient::builder()
        .with_root_certificates(root_store)
        .without_cert()
        .with_alpns(vec!["h3"])
        .build();
    let quic_client = Arc::new(quic_client);

    // Create RootState
    let state = Arc::new(tokio::sync::Mutex::new(pishoo::root_state::RootState::new(
        listeners.clone(),
        quic_client,
    )));

    // Write PID file (root only)
    signal::init_pid_file(&pid_file).await?;

    let mut local_runtimes = register_local_runtimes(&state, &current_entry_config).await?;

    spawn_configured_workers(&state, current_worker_targets.clone()).await?;

    let initial_publish_configs = gateway::dns::build_publish_configs(&publishable_servers)
        .whatever_context("failed to build dns publish configs")?;
    let mut _publisher = spawn_publisher(listeners.clone(), initial_publish_configs).await;

    // Create signal handler once — reused across the main loop so that signals
    // arriving during reload are never lost.
    let mut signals = signal::RootSignalHandler::new()?;

    // Central accept loop: route connections by server_name
    let accept_state = state.clone();
    let accept_listeners = listeners.clone();
    let accept_handle = tokio::spawn(
        async move {
            loop {
                let (conn, server_name, _pathway, _link) = match accept_listeners.accept().await {
                    Ok(incoming) => incoming,
                    Err(error) => {
                        tracing::error!(error = %Report::from_error(&error), "accept loop error");
                        break;
                    }
                };

                let sender = {
                    let st = accept_state.lock().await;
                    st.get_conn_sender(&server_name)
                };
                let Some(sender) = sender else {
                    tracing::warn!(%server_name, "no listener registered for connection");
                    continue;
                };

                if sender.send(conn).await.is_err() {
                    tracing::warn!(%server_name, "failed to route connection (channel closed)");
                }
            }
        }
        .in_current_span(),
    );

    let monitor_state = state.clone();
    let monitor_handle = tokio::spawn(
        async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                let mut st = monitor_state.lock().await;
                let exited = st.collect_exited_workers();
                for pid in exited {
                    st.cleanup_worker_with_reason(pid, "child_exit");
                }
            }
        }
        .in_current_span(),
    );

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
                // Try to forward the shutdown signal to workers.  Use try_lock
                // to avoid blocking when the state mutex is held by a slow
                // operation (e.g. add_server during request_listen).  Workers
                // running in the same process group already receive the terminal
                // signal directly; the explicit forward is a best-effort courtesy.
                match state.try_lock() {
                    Ok(mut st) => {
                        st.forward_unix_signal(forwarded);
                    }
                    Err(_) => {
                        tracing::warn!(
                            "state mutex held; skipping signal forward (workers will be force-killed)"
                        );
                    }
                }
                accept_handle.abort();
                tracing::info!(?sig, "forwarded shutdown signal to workers");

                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
                loop {
                    let mut done = false;
                    {
                        let mut st = state.lock().await;
                        let exited = st.collect_exited_workers();
                        for pid in exited {
                            st.cleanup_worker_with_reason(pid, "signal_terminate");
                        }
                        if st.worker_pids().is_empty() {
                            done = true;
                        }
                    }
                    if done || tokio::time::Instant::now() >= deadline {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }

                let force_killed = {
                    let mut st = state.lock().await;
                    if st.worker_pids().is_empty() {
                        Vec::new()
                    } else {
                        st.force_kill_workers("shutdown_timeout")
                    }
                };
                if !force_killed.is_empty() {
                    tracing::warn!(
                        workers = force_killed.len(),
                        "force-killed lingering workers after shutdown timeout"
                    );

                    let force_kill_deadline =
                        tokio::time::Instant::now() + std::time::Duration::from_secs(2);
                    loop {
                        let mut done = false;
                        {
                            let mut st = state.lock().await;
                            let exited = st.collect_exited_workers();
                            for pid in exited {
                                st.cleanup_worker_with_reason(pid, "forced_shutdown");
                            }
                            if st.worker_pids().is_empty() {
                                done = true;
                            }
                        }
                        if done || tokio::time::Instant::now() >= force_kill_deadline {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
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

                if let Err(error) =
                    replace_local_runtimes(&state, &mut local_runtimes, &next_snapshot.entry_config)
                        .await
                {
                    tracing::warn!(
                        error = %Report::from_error(&error),
                        "failed to reload root-local servers; keeping previous worker state"
                    );
                    continue;
                }

                {
                    let mut st = state.lock().await;
                    st.forward_unix_signal(Signal::SIGHUP);
                }
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
                {
                    let mut st = state.lock().await;
                    st.forward_unix_signal(Signal::SIGUSR1);
                }
                tracing::info!("root log reopened, forwarded reopen signal to workers");
            }
        }
    }

    _publisher = None;
    accept_handle.abort();
    let _ = accept_handle.await;
    monitor_handle.abort();
    let _ = monitor_handle.await;
    for runtime in local_runtimes.drain(..) {
        runtime.stop();
    }
    match tokio::time::timeout(Duration::from_secs(1), state.lock()).await {
        Ok(mut st) => {
            for pid in st.worker_pids() {
                st.cleanup_worker_with_reason(pid, "root_shutdown");
            }
        }
        Err(_) => {
            tracing::warn!("state lock timeout during final cleanup; skipping worker cleanup");
        }
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

async fn register_local_runtimes(
    state: &Arc<tokio::sync::Mutex<pishoo::root_state::RootState>>,
    entry_config: &pishoo::config::EntryConfig,
) -> Result<Vec<pishoo::local_service::LocalServerRuntime>, Whatever> {
    let local_policy =
        pishoo::policy::load_policy_bundle(entry_config.local_access_rules_uri.as_deref())
            .await
            .whatever_context("failed to load root-local access rules")?;

    if entry_config.local_servers.is_empty() {
        return Ok(Vec::new());
    }

    pishoo::local_service::register_local_servers(
        state,
        &entry_config.local_servers,
        local_policy.location_rules,
    )
    .await
    .whatever_context("failed to register root-local servers")
}

async fn replace_local_runtimes(
    state: &Arc<tokio::sync::Mutex<pishoo::root_state::RootState>>,
    local_runtimes: &mut Vec<pishoo::local_service::LocalServerRuntime>,
    entry_config: &pishoo::config::EntryConfig,
) -> Result<(), Whatever> {
    let retired = {
        let mut st = state.lock().await;
        st.retire_local_servers()
    };
    if !retired.is_empty() {
        tracing::info!(
            servers = retired.len(),
            "retired root-local servers before reload"
        );
    }

    for runtime in local_runtimes.drain(..) {
        runtime.stop();
    }

    *local_runtimes = register_local_runtimes(state, entry_config).await?;
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

async fn spawn_configured_workers(
    state: &Arc<tokio::sync::Mutex<pishoo::root_state::RootState>>,
    worker_targets: Vec<pishoo::config::ResolvedWorkerTarget>,
) -> Result<(), Whatever> {
    if worker_targets.is_empty() {
        return Ok(());
    }

    let worker_bin =
        std::env::current_exe().whatever_context("failed to determine current executable path")?;
    let worker_bin = worker_bin.parent().unwrap().join("pishoo-worker");

    for target in worker_targets {
        let spawned = pishoo::worker_spawn::spawn_worker(
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
        let pid = spawned.handle.pid().expect("worker must have pid");

        ensure_worker_identity(&target, pid, &spawned.hello)?;

        let mut st = state.lock().await;
        st.register_worker(Pid::from_raw(pid as i32), target.uid, spawned.handle);
    }

    Ok(())
}

fn ensure_worker_identity(
    target: &pishoo::config::ResolvedWorkerTarget,
    pid: u32,
    hello: &pishoo::protocol::WorkerHello,
) -> Result<(), Whatever> {
    if hello.pid != pid
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
        pid,
        uid = target.uid.as_raw(),
        gid = target.gid.as_raw(),
        user = %target.username,
        "worker privilege separation verified"
    );

    Ok(())
}
