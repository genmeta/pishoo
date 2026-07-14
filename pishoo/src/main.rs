#![cfg(unix)]

use std::{path::PathBuf, sync::Arc};

use clap::Parser;
use dhttp::network::DhttpNetwork;
use gateway::error::Whatever;
use nix::sys::signal::Signal;
use pishoo::hypervisor::signal;
use snafu::{FromString, ResultExt};
use tokio::fs;
use tokio_util::sync::CancellationToken;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Set standalone configuration file. When omitted, pishoo loads the global DHTTP home.
    #[arg(short)]
    config_file: Option<PathBuf>,
    /// Send signal to a master process (only on Linux/MacOS)
    #[arg(short, default_value = None)]
    signal: Option<signal::SignalType>,
    /// Test configuration and exit
    #[arg(short, default_value_t = false)]
    test_config: bool,
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

    let config_source = pishoo::config::PishooConfigSource::resolve(args.config_file)
        .whatever_context("failed to resolve pishoo configuration source")?;
    let config_file = config_source.config_path().to_path_buf();

    let initial_plan = pishoo::config::load_global_pishoo_plan(&config_source).await;
    if args.test_config || args.signal.is_some() {
        let plan = initial_plan.map_err(|error| {
            tracing::error!(
                error = %snafu::Report::from_error(&error),
                "failed to load configuration"
            );
            Whatever::with_source(Box::new(error), "failed to load configuration".to_owned())
        })?;
        let pid_file = pishoo::config::pid_path(plan.pishoo());
        if args.test_config {
            tracing::info!(path = %config_file.display(), "configuration parsed successfully");
            return Ok(());
        }
        return signal::send_signal(&pid_file, args.signal.expect("signal checked above")).await;
    }

    let plan = match initial_plan {
        Ok(plan) => Some(plan),
        Err(error) => {
            tracing::error!(
                error = %snafu::Report::from_error(&error),
                "global pishoo configuration failed; starting in config-failed state"
            );
            None
        }
    };
    let pid_file = plan.as_ref().map_or_else(
        || PathBuf::from(pishoo::config::PID_FILE_DEFAULT),
        |plan| pishoo::config::pid_path(plan.pishoo()),
    );
    let mut current_worker_targets = Vec::new();

    tracing::info!(
        pid_file = %pid_file.display(),
        "pishoo starting"
    );

    // --- Multi-process supervisor setup ---

    // Build the shared Network used by every registered SNI. Workers register
    // by calling back through IPC (request_listen) — no servers are added up-front.
    let network = DhttpNetwork::builder()
        .build()
        .await
        .whatever_context("failed to build dhttp network")?;

    // Create RootState (interior mutability — no external Mutex needed)
    let state = Arc::new(pishoo::hypervisor::state::RootState::new(network));

    // Write PID file (root only)
    signal::init_pid_file(&pid_file).await?;

    let mut global_service_handle = None;
    if let Some(plan) = plan {
        current_worker_targets = plan.desired_workers().to_vec();
        state
            .set_desired_workers(current_worker_targets.clone())
            .await;
        let global_branch = pishoo::hypervisor::global_service::spawn_global_service(&state, &plan);
        let worker_branch = pishoo::hypervisor::process::spawn_configured_workers(
            &state,
            current_worker_targets.clone(),
            plan.worker_defaults().clone(),
        );
        let (service, ()) = tokio::join!(global_branch, worker_branch);
        global_service_handle = service;
    }

    // Create signal handler once — reused across the main loop so that signals
    // arriving during reload are never lost.
    let mut signals = signal::RootSignalHandler::new()?;

    // Per-server accept tasks are spawned through listener acquisition; no
    // central accept loop is needed here.

    let monitor_shutdown = CancellationToken::new();
    let monitor_handle =
        pishoo::hypervisor::process::spawn_monitor_loop(state.clone(), monitor_shutdown.clone());

    tracing::info!("pishoo ready");

    loop {
        let sig = tokio::select! {
            sig = signals.wait() => sig,
            name = pishoo::hypervisor::global_service::wait_global_service_completion(
                &mut global_service_handle,
            ) => {
                global_service_handle
                    .as_mut()
                    .expect("service completion requires a global service handle")
                    .handle_service_exit(name)
                    .await;
                tracing::warn!("global server service exited; released its resources");
                continue;
            }
        };

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
                pishoo::hypervisor::shutdown::run_shutdown(&state, forwarded).await;
                break;
            }
            signal::RootSignal::SigHup => {
                pishoo::hypervisor::reload::run_reload(
                    &state,
                    &config_source,
                    &mut current_worker_targets,
                    &mut global_service_handle,
                )
                .await;
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

    monitor_shutdown.cancel();
    let _ = monitor_handle.await;
    if let Some(handle) = global_service_handle.take() {
        handle.shutdown().await;
    }
    state.cleanup_local_resources().await;
    for pid in state.worker_pids().await {
        state
            .cleanup_worker(
                pid,
                pishoo::hypervisor::state::WorkerProcessError::RootShutdown,
            )
            .await;
    }
    state.wait_resource_transitions().await;
    _ = fs::remove_file(&pid_file).await;
    Ok(())
}
