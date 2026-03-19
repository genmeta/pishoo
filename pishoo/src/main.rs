#![cfg(unix)]

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use clap::Parser;
use gateway::error::Whatever;
use genmeta_home::GenmetaHome;
use gm_quic::prelude::{QuicListeners, handy::server_parameters};
use nix::{
    sys::signal::Signal,
    unistd::{Pid, Uid},
};
use rustls::server::WebPkiClientVerifier;
use snafu::{OptionExt, Report, ResultExt};
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

    let _genmeta_home =
        GenmetaHome::load_from_environment().whatever_context("failed to load Genmeta home directory")?;

    let config_file = args.config_file;

    let config = fs::read(&config_file).await.whatever_context(format!(
        "failed to read configuration file at `{}`",
        config_file.display()
    ))?;
    let config = gateway::parse::parse(&config, config_file.parent()).whatever_context(format!(
        "failed to parse configuration file at `{}`",
        config_file.display()
    ))?;

    let root_config = pishoo::config::parse_root_config(&config)
        .whatever_context("failed to parse root worker configuration")?;

    if args.test_config {
        tracing::info!(
            path = %config_file.display(),
            "configuration syntax is ok"
        );
        return Ok(());
    }

    let pid_file = root_config.pid_file.as_str();

    if let Some(signal) = args.signal {
        return signal::send_signal(pid_file, signal).await;
    }

    // --- Multi-process supervisor setup ---

    // Create QuicListeners (empty — workers add servers via request_listen)
    let roots = pishoo::tls::root_cert_store();
    let tls_client_cert_verifier = WebPkiClientVerifier::builder(roots)
        .allow_unauthenticated()
        .build()
        .expect("failed to build tls client cert verifier");

    let listeners = QuicListeners::builder()
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
    signal::init_pid_file(pid_file).await?;

    let worker_targets = pishoo::config::resolve_worker_targets(&root_config)
        .whatever_context("failed to resolve configured worker users")?;

    let worker_bin =
        std::env::current_exe().whatever_context("failed to determine current executable path")?;
    let worker_bin = worker_bin.parent().unwrap().join("pishoo-worker");

    for target in worker_targets {
        let spawned = pishoo::worker_spawn::spawn_worker(
            &worker_bin,
            Uid::from_raw(target.uid),
            target.gid,
            target.username.clone(),
            target.home.clone(),
            target.log_dir.clone(),
            state.clone(),
        )
        .await
        .whatever_context(format!("failed to spawn worker for user `{}`", target.username))?;
        let pid = spawned.handle.pid().expect("worker must have pid");
        let target_uid = Uid::from_raw(target.uid);
        let hello = spawned.hello;
        if hello.pid != pid
            || hello.uid != target_uid.as_raw()
            || hello.euid != target_uid.as_raw()
            || hello.gid != target.gid
            || hello.egid != target.gid
        {
            snafu::whatever!(
                "worker identity mismatch for user `{}`: pid={} hello_pid={} uid/euid={}/{} expected_uid={} gid/egid={}/{} expected_gid={}",
                target.username,
                pid,
                hello.pid,
                hello.uid,
                hello.euid,
                target_uid.as_raw(),
                hello.gid,
                hello.egid,
                target.gid
            );
        }

        tracing::info!(
            pid,
            uid = target_uid.as_raw(),
            gid = target.gid,
            user = %target.username,
            "worker privilege separation verified"
        );

        let mut st = state.lock().await;
        st.register_worker(Pid::from_raw(pid as i32), target_uid, spawned.handle);
    }

    // Central accept loop: route connections by server_name
    let accept_state = state.clone();
    let accept_listeners = listeners.clone();
    let accept_handle = tokio::spawn(async move {
        loop {
            match accept_listeners.accept().await {
                Ok((conn, server_name, _pathway, _link)) => {
                    let st = accept_state.lock().await;
                    if let Some(sender) = st.get_conn_sender(&server_name) {
                        if sender.send(conn).await.is_err() {
                            tracing::warn!(%server_name, "failed to route connection (channel closed)");
                        }
                    } else {
                        tracing::warn!(%server_name, "no listener registered for connection");
                    }
                }
                Err(error) => {
                    tracing::error!(error = %Report::from_error(&error), "accept loop error");
                    break;
                }
            }
        }
    }
    .in_current_span());

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
        let Some(sig) = signal::handle_signal().await? else {
            break;
        };

        let forwarded = match sig {
            signal::RootSignal::SigTerm => Signal::SIGTERM,
            signal::RootSignal::SigInt => Signal::SIGINT,
            signal::RootSignal::SigQuit => Signal::SIGQUIT,
            signal::RootSignal::SigHup => Signal::SIGHUP,
            signal::RootSignal::SigUsr1 => {
                reopen_root_log();
                Signal::SIGUSR1
            }
        };
        {
            let mut st = state.lock().await;
            st.forward_unix_signal(forwarded);
        }

        match sig {
            signal::RootSignal::SigTerm
            | signal::RootSignal::SigInt
            | signal::RootSignal::SigQuit => {
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
                break;
            }
            signal::RootSignal::SigHup => {
                tracing::info!("forwarded reload signal to workers");
            }
            signal::RootSignal::SigUsr1 => {
                tracing::info!("root log reopened, forwarded reopen signal to workers");
            }
        }
    }

    monitor_handle.abort();
    {
        let mut st = state.lock().await;
        for pid in st.worker_pids() {
            st.cleanup_worker_with_reason(pid, "root_shutdown");
        }
    }
    _ = fs::remove_file(pid_file).await;
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
