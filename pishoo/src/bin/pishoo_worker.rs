//! pishoo-worker: per-user worker process.
//!
//! Spawned by the root pishoo process with stdin/stdout piped for remoc IPC.
//! Receives [`WorkerBootstrap`] from the root (containing a
//! [`pishoo::ipc::ControlPlaneClient`]), scans `~/.genmeta` identities, builds a
//! [`pishoo::service::ServiceConfig`], and calls [`run_service()`] — the same generic
//! code path used by root-local services.
//!
//! **stdout is reserved for remoc transport** — all logging goes to stderr.

use gateway::error::Whatever;
use genmeta_home::GenmetaHome;
use pishoo::{
    ipc::{WorkerBootstrap, WorkerHello},
    service::run_service,
    worker::{config::build_service_config, remote_plane::RemoteControlPlane},
};
use snafu::{OptionExt, Report, ResultExt};
use tracing::Instrument;

#[tokio::main(flavor = "current_thread")]
#[snafu::report]
async fn main() -> Result<(), Whatever> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    // Establish remoc connection over stdin (read) / stdout (write).
    let (conn, mut base_tx, mut base_rx): (
        _,
        remoc::rch::base::Sender<WorkerHello>,
        remoc::rch::base::Receiver<WorkerBootstrap>,
    ) = remoc::Connect::io(remoc::Cfg::default(), stdin, stdout)
        .await
        .whatever_context("failed to establish remoc transport")?;
    tokio::spawn(conn.in_current_span());

    // Receive bootstrap payload from root.
    let bootstrap = base_rx
        .recv()
        .await
        .whatever_context("failed to receive worker bootstrap")?
        .whatever_context("root closed channel without sending bootstrap")?;

    // Logging must go to stderr (stdout is the remoc transport).
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    tracing::info!(
        uid = bootstrap.uid,
        username = %bootstrap.username,
        home = %bootstrap.home.display(),
        "Worker bootstrap received"
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
    tracing::info!("Startup hello sent");

    // Create the RemoteControlPlane from the bootstrap's ControlPlane client.
    let plane = RemoteControlPlane::new(bootstrap.control_plane);

    // Scan identities and build service config.
    let genmeta_home = GenmetaHome::new(bootstrap.home.join(".genmeta"));

    let config = build_service_config(&genmeta_home)
        .await
        .whatever_context("failed to build service config")?;

    tracing::info!(
        servers = config.servers.len(),
        "Service config built, starting service"
    );

    // Run the service with signal-based shutdown.
    let mut term_signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .whatever_context("failed to create SIGTERM listener")?;
    let mut int_signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .whatever_context("failed to create SIGINT listener")?;
    let mut quit_signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::quit())
        .whatever_context("failed to create SIGQUIT listener")?;
    let mut hup_signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .whatever_context("failed to create SIGHUP listener")?;

    tokio::select! {
        result = run_service(&plane, &config) => {
            if let Err(error) = result {
                tracing::error!(
                    error = %Report::from_error(error.as_ref()),
                    "Service exited with error"
                );
            }
        }
        _ = term_signal.recv() => {
            tracing::info!("Received SIGTERM, shutting down");
        }
        _ = int_signal.recv() => {
            tracing::info!("Received SIGINT, shutting down");
        }
        _ = quit_signal.recv() => {
            tracing::info!("Received SIGQUIT, shutting down");
        }
        _ = hup_signal.recv() => {
            // TODO: implement config reload — requires adding release_listen()
            // to the ControlPlane trait to properly unregister listeners before
            // re-scanning identities.
            tracing::warn!("Received SIGHUP, reload not yet implemented");
        }
    }

    tracing::info!("pishoo-worker exiting");
    Ok(())
}
