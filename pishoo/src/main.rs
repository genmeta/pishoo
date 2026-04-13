#![cfg(unix)]

use std::{path::PathBuf, sync::Arc};

use clap::Parser;
use dquic::prelude::{QuicListeners, handy::server_parameters};
use gateway::error::Whatever;
use nix::sys::signal::Signal;
use rustls::server::WebPkiClientVerifier;
use snafu::{Report, ResultExt};
use tokio::fs;
use tracing::Instrument;

mod signal;

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
        tracing::info!(
            path = %config_file.display(),
            "configuration parsed successfully"
        );
        return Ok(());
    }

    let pid_file = entry_config.pid_file.clone();

    if let Some(signal) = args.signal {
        return signal::send_signal(&pid_file, signal).await;
    }

    let current_entry_config = entry_config;
    let mut current_worker_targets =
        pishoo::config::resolve_entry_worker_targets(&current_entry_config)
            .whatever_context("failed to resolve configured worker users")?;

    tracing::info!(
        pid_file = %current_entry_config.pid_file,
        "pishoo starting"
    );

    // --- Multi-process supervisor setup ---

    // Create QuicListeners (empty — workers add servers via request_listen)
    let roots = pishoo::tls::root_cert_store();
    let tls_client_cert_verifier = WebPkiClientVerifier::builder(roots)
        .allow_unauthenticated()
        .build()
        .expect("failed to build tls client cert verifier");

    let listeners = QuicListeners::builder()
        .with_resolver(Arc::new(gateway::dns::build_query_resolver_chain(&[])))
        .with_stun(gateway::dns::DEFAULT_STUN_SERVER)
        .with_parameters(server_parameters())
        .with_client_cert_verifier(tls_client_cert_verifier)
        .with_alpns([b"h3".as_slice()])
        .listen(1024)
        .expect("failed to create QuicListeners");

    // Create RootState (interior mutability — no external Mutex needed)
    let state = Arc::new(pishoo::hypervisor::state::RootState::new(listeners.clone()));

    // Write PID file (root only)
    signal::init_pid_file(&pid_file).await?;

    let mut local_service_handle =
        pishoo::hypervisor::local_service::spawn_local_service(&state, &current_entry_config)
            .await?;
    drop(current_entry_config);

    pishoo::hypervisor::process::spawn_configured_workers(&state, current_worker_targets.clone())
        .await;

    // Create signal handler once — reused across the main loop so that signals
    // arriving during reload are never lost.
    let mut signals = signal::RootSignalHandler::new()?;

    // Central accept loop: route connections by server_name
    let accept_handle = pishoo::hypervisor::network::spawn_accept_loop(state.clone());

    let monitor_handle = pishoo::hypervisor::process::spawn_monitor_loop(state.clone());

    // Watch for network interface changes and reconcile bind URIs
    let network_watch_handle = pishoo::hypervisor::network::spawn_network_watch_loop(state.clone());

    tracing::info!("pishoo ready");

    loop {
        let sig = signals.wait().await;

        match sig {
            signal::RootSignal::SigTerm
            | signal::RootSignal::SigInt
            | signal::RootSignal::SigQuit => {
                tracing::info!(?sig, "received shutdown signal");
                let forwarded = match sig {
                    signal::RootSignal::SigTerm => Signal::SIGTERM,
                    signal::RootSignal::SigInt => Signal::SIGINT,
                    signal::RootSignal::SigQuit => Signal::SIGQUIT,
                    _ => unreachable!("matched shutdown signals only"),
                };
                state.forward_unix_signal(forwarded).await;
                accept_handle.abort();

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
                } else {
                    tracing::info!("all workers exited gracefully");
                }
                tracing::info!("shutdown complete");
                break;
            }
            signal::RootSignal::SigHup => {
                tracing::info!("received reload signal");
                let next_snapshot =
                    match pishoo::hypervisor::reload::load_root_reload_snapshot(&config_file).await
                    {
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

                if let Err(error) = pishoo::hypervisor::local_service::replace_local_service(
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

                // Compute worker diff.
                let diff = pishoo::config::compute_worker_diff(
                    &current_worker_targets,
                    &next_snapshot.worker_targets,
                );

                fn target_names(targets: &[pishoo::config::ResolvedWorkerTarget]) -> Vec<&str> {
                    targets.iter().map(|t| t.name.as_str()).collect()
                }
                tracing::info!(
                    unchanged = ?target_names(&diff.unchanged),
                    added = ?target_names(&diff.added),
                    removed = ?target_names(&diff.removed),
                    changed = ?diff.changed.iter().map(|(_, new)| new.name.as_str()).collect::<Vec<_>>(),
                    "reload diff"
                );

                // Phase 1: Kill removed + changed workers (parallel).
                let workers_to_kill: Vec<_> = diff
                    .removed
                    .iter()
                    .map(|t| (t.uid, "reload_removed"))
                    .chain(
                        diff.changed
                            .iter()
                            .map(|(old, _)| (old.uid, "reload_changed")),
                    )
                    .collect();

                if !workers_to_kill.is_empty() {
                    let mut kill_tasks = tokio::task::JoinSet::new();
                    for (uid, reason) in &workers_to_kill {
                        let state = state.clone();
                        let uid = *uid;
                        let reason = *reason;
                        kill_tasks.spawn(
                            async move {
                                if let Some(pid) = state.pid_for_uid(uid).await {
                                    // SIGTERM + 2s grace
                                    state.send_signal_to_user(uid, Signal::SIGTERM).await;
                                    if !state
                                        .wait_worker_exit(pid, std::time::Duration::from_secs(2))
                                        .await
                                    {
                                        state.force_kill_worker(pid).await;
                                        state
                                            .wait_worker_exit(
                                                pid,
                                                std::time::Duration::from_secs(2),
                                            )
                                            .await;
                                    }
                                    state.cleanup_worker_with_reason(pid, reason).await;
                                }
                            }
                            .in_current_span(),
                        );
                    }
                    while kill_tasks.join_next().await.is_some() {}
                }

                // Scrub conflicted names before forwarding reload to workers.
                state.scrub_conflicts().await;

                // Phase 2: Forward SIGHUP to unchanged workers.
                for target in &diff.unchanged {
                    state.send_signal_to_user(target.uid, Signal::SIGHUP).await;
                }

                // Phase 3: Spawn added + changed workers.
                let workers_to_spawn: Vec<_> = diff
                    .added
                    .into_iter()
                    .chain(diff.changed.into_iter().map(|(_old, new)| new))
                    .collect();
                if !workers_to_spawn.is_empty() {
                    pishoo::hypervisor::process::spawn_configured_workers(&state, workers_to_spawn)
                        .await;
                }

                current_worker_targets = next_snapshot.worker_targets;
                tracing::info!("reload complete");
            }
            signal::RootSignal::SigUsr1 => {
                pishoo::hypervisor::log::reopen_root_log();
                state.forward_unix_signal(Signal::SIGUSR1).await;
                tracing::info!("root log reopened, forwarded reopen signal to workers");
            }
            signal::RootSignal::SigChld => {
                state.worker_notify.notify_one();
            }
        }
    }

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
