#![cfg(unix)]

use std::{path::PathBuf, sync::Arc};

use clap::Parser;
use gateway::error::Whatever;
use h3x::dquic::{Network, param::handy::server_parameters, server::ServerQuicConfig};
use nix::sys::signal::Signal;
use pishoo::hypervisor::signal;
use rustls::server::WebPkiClientVerifier;
use snafu::{FromString, ResultExt};
use tokio::fs;
use tokio_util::sync::CancellationToken;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Set configuration file
    #[arg(short, default_value = "/etc/pishoo/pishoo.conf")]
    config_file: PathBuf,
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

    let config_file = args.config_file;

    let registry = gateway::parse::default_registry();
    let config = match gateway::parse::load_config_file(
        &config_file,
        &registry,
        gateway::parse::registry::BuildOptions::default(),
    )
    .await
    {
        Ok(config) => config,
        Err(failure) => {
            tracing::error!(
                error = %snafu::Report::from_error(&failure.error),
                diagnostic = %failure.diagnostic(),
                "failed to load configuration"
            );
            return Err(Whatever::with_source(
                Box::new(failure),
                "failed to load configuration".to_owned(),
            ));
        }
    };

    let entry_config = pishoo::config::parse_entry_config(&config.root)
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
        pid_file = %current_entry_config.pid_file.display(),
        "pishoo starting"
    );

    // --- Multi-process supervisor setup ---

    // Build the shared Network + default ServerQuicConfig that every
    // registered SNI will reuse. Workers register by calling back through
    // IPC (request_listen) — no servers are added up-front.
    let roots = pishoo::tls::root_cert_store();
    let tls_client_cert_verifier = WebPkiClientVerifier::builder(roots)
        .allow_unauthenticated()
        .build()
        .expect("failed to build tls client cert verifier");

    let server_qcfg = ServerQuicConfig {
        parameters: server_parameters(),
        alpns: vec![b"h3".to_vec()],
        backlog: 1024,
        client_cert_verifier: tls_client_cert_verifier,
        ..Default::default()
    };

    let network = Network::builder()
        .stun_server(Arc::<str>::from(gateway::dns::DEFAULT_STUN_SERVER))
        .build();

    // Create RootState (interior mutability — no external Mutex needed)
    let state = Arc::new(pishoo::hypervisor::state::RootState::new(
        network,
        server_qcfg,
    ));

    // Write PID file (root only)
    signal::init_pid_file(&pid_file).await?;

    let mut local_service_handle = pishoo::hypervisor::local_service::spawn_local_service(
        &state,
        &current_entry_config,
        std::collections::HashMap::new(),
    )
    .await?;
    drop(current_entry_config);

    pishoo::hypervisor::process::spawn_configured_workers(&state, current_worker_targets.clone())
        .await;

    // Create signal handler once — reused across the main loop so that signals
    // arriving during reload are never lost.
    let mut signals = signal::RootSignalHandler::new()?;

    // Per-server accept tasks are spawned inside `register_listener`; no
    // central accept loop is needed here.

    let monitor_shutdown = CancellationToken::new();
    let monitor_handle =
        pishoo::hypervisor::process::spawn_monitor_loop(state.clone(), monitor_shutdown.clone());

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
                pishoo::hypervisor::shutdown::run_shutdown(&state, forwarded).await;
                break;
            }
            signal::RootSignal::SigHup => {
                pishoo::hypervisor::reload::run_reload(
                    &state,
                    &config_file,
                    &mut current_worker_targets,
                    &mut local_service_handle,
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
    if let Some(handle) = local_service_handle.take() {
        handle.shutdown().await;
    }
    state.cleanup_local_resources().await;
    for pid in state.worker_pids().await {
        state.cleanup_worker_with_reason(pid, "root_shutdown").await;
    }
    _ = fs::remove_file(&pid_file).await;
    Ok(())
}
