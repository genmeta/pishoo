//! pishoo-worker: per-user worker process.
//!
//! Spawned by the root pishoo process with a MuxChannel socketpair on FD 3.
//! Receives [`WorkerBootstrap`] from the root (containing a
//! [`pishoo::ipc::ControlPlaneClient`]), scans `~/.dhttp` identities, builds a
//! [`pishoo::service::ServiceConfig`], and calls [`run_service()`] — the same generic
//! code path used by root-local services.
//!
//! **FD 3 is reserved for MuxChannel transport** — all logging goes to stderr.

use std::sync::Arc;

use dhttp_home::DhttpHome;
use gateway::error::Whatever;
use h3x::ipc::transport::MuxChannel;
use pishoo::{
    ipc::{WorkerBootstrap, WorkerHello},
    service::{self, PreparedServer, setup_service},
    worker::{config::build_service_config, remote_plane::RemoteControlPlane},
};
use snafu::{OptionExt, Report, ResultExt};
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};
use tracing::Instrument;

#[tokio::main]
#[snafu::report]
async fn main() -> Result<(), Whatever> {
    let user = std::env::var("PISHOO_USER").unwrap_or_else(|_| {
        eprintln!("PISHOO_USER not set; this binary must be spawned by pishoo root");
        std::process::exit(1);
    });
    let _tracing_guard = pishoo::tracing_init::init_tracing(&format!(
        "pishoo-worker:{}/{}",
        user,
        std::process::id()
    ));

    // Recover the MuxChannel FD from FD 3 (dup2'd by root in child_exec).
    let mux_fd = {
        use std::os::fd::FromRawFd;
        // SAFETY: the root process dup2'd the socketpair FD to FD 3 in child_exec
        // before execve. FD 3 is guaranteed to be open and valid.
        unsafe { std::os::fd::OwnedFd::from_raw_fd(3) }
    };

    let mux =
        MuxChannel::from_fd(mux_fd).whatever_context("failed to create MuxChannel from fd 3")?;
    let (sink, stream) = mux.split().whatever_context("failed to split MuxChannel")?;

    // Keep FdRegistry for receiving FDs from root (e.g. session child FDs).
    let fd_registry = stream.fd_registry();

    // Establish remoc connection over MuxSink/MuxStream.
    let (conn, mut base_tx, mut base_rx): (
        _,
        remoc::rch::base::Sender<WorkerHello>,
        remoc::rch::base::Receiver<WorkerBootstrap>,
    ) = remoc::Connect::framed(remoc::Cfg::default(), sink, stream)
        .await
        .whatever_context("failed to establish remoc transport")?;
    let _conn_handle = AbortOnDropHandle::new(tokio::spawn(conn.in_current_span()));

    // Receive bootstrap payload from root.
    let bootstrap = base_rx
        .recv()
        .await
        .whatever_context("failed to receive worker bootstrap")?
        .whatever_context("root closed channel without sending bootstrap")?;

    tracing::debug!(
        uid = bootstrap.uid,
        username = %bootstrap.username,
        home = %bootstrap.home.display(),
        "bootstrap received"
    );

    // Send startup hello back to root.
    let hello = WorkerHello {
        pid: std::process::id(),
        uid: nix::unistd::getuid().as_raw(),
        euid: nix::unistd::geteuid().as_raw(),
        gid: nix::unistd::getgid().as_raw(),
        egid: nix::unistd::getegid().as_raw(),
    };
    base_tx
        .send(hello)
        .await
        .whatever_context("failed to send startup hello")?;
    tracing::debug!("startup hello sent");

    // Create the RemoteControlPlane from the bootstrap's ControlPlane client.
    // Pass FdRegistry so spawn_session can receive FDs from root via MuxChannel.
    let plane = Arc::new(RemoteControlPlane::new(
        bootstrap.control_plane,
        fd_registry,
    ));

    let dhttp_home = DhttpHome::new(bootstrap.home.join(".dhttp"));

    let mut config = build_service_config(&dhttp_home)
        .await
        .whatever_context("failed to build service config")?;

    let mut term_signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .whatever_context("failed to create SIGTERM listener")?;
    let mut int_signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .whatever_context("failed to create SIGINT listener")?;
    let mut quit_signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::quit())
        .whatever_context("failed to create SIGQUIT listener")?;
    let mut hup_signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .whatever_context("failed to create SIGHUP listener")?;

    let mut prepared: Vec<PreparedServer<_>> = Vec::new();

    loop {
        // Collect reusable listeners from the previous cycle.
        let existing_listeners =
            service::collect_reusable_listeners(std::mem::take(&mut prepared), &config).await;

        tracing::debug!(
            servers = config.servers.len(),
            "service config built, starting service"
        );

        prepared = match setup_service(&*plane, &config, existing_listeners).await {
            Ok(p) => p,
            Err(error) => {
                tracing::error!(
                    error = %Report::from_error(&error),
                    "failed to set up service"
                );
                break;
            }
        };

        let shutdown = CancellationToken::new();
        let mut next_config = None;
        let mut should_exit = false;

        {
            let router_state = gateway::reverse::router::RouterState {
                #[cfg(feature = "sshd")]
                session_spawner: plane.clone(),
            };
            let service = service::run_service(
                &mut prepared,
                &config.h3_settings,
                &config.access_rules,
                router_state,
                shutdown.clone(),
            );
            tokio::pin!(service);

            tokio::select! {
                () = &mut service => {
                    tracing::info!("service exited");
                    should_exit = true;
                }
                _ = term_signal.recv() => {
                    tracing::info!(signal = "SIGTERM", "received shutdown signal");
                    shutdown.cancel();
                    service.await;
                    should_exit = true;
                }
                _ = int_signal.recv() => {
                    tracing::info!(signal = "SIGINT", "received shutdown signal");
                    shutdown.cancel();
                    service.await;
                    should_exit = true;
                }
                _ = quit_signal.recv() => {
                    tracing::info!(signal = "SIGQUIT", "received shutdown signal");
                    shutdown.cancel();
                    service.await;
                    should_exit = true;
                }
                _ = hup_signal.recv() => {
                    tracing::info!("received reload signal");
                    let rebuilt_config = match build_service_config(&dhttp_home).await {
                        Ok(config) => config,
                        Err(error) => {
                            tracing::warn!(
                                error = %Report::from_error(&error),
                                "failed to rebuild service config, keeping current service"
                            );
                            // Cancel and recover listeners before continuing
                            // so they are not leaked.
                            shutdown.cancel();
                            service.await;
                            continue;
                        }
                    };

                    shutdown.cancel();
                    service.await;
                    next_config = Some(rebuilt_config);
                }
            }
        }

        if should_exit {
            break;
        }

        if let Some(rebuilt_config) = next_config {
            config = rebuilt_config;
            tracing::info!(servers = config.servers.len(), "reload complete");
        }
    }

    tracing::info!("exiting");
    Ok(())
}
