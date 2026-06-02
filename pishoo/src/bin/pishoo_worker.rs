//! pishoo-worker: per-user worker process.
//!
//! Spawned by the root pishoo process with a MuxChannel socketpair on FD 3.
//! Receives [`WorkerBootstrap`] from the root (containing a
//! [`pishoo::ipc::ControlPlaneClient`]), scans `~/.dhttp` identities, and drives
//! [`pishoo::service::runtime::WorkerRuntime`] over the root-provided control
//! plane.
//!
//! **FD 3 is reserved for MuxChannel transport** — all logging goes to stderr.

use std::sync::Arc;

use dhttp_home::DhttpHome;
use gateway::error::Whatever;
use h3x::ipc::transport::MuxChannel;
use pishoo::{
    ipc::{WorkerBootstrap, WorkerHello},
    worker::remote_plane::RemoteControlPlane,
};
use snafu::{OptionExt, Report, ResultExt};
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

    // Keep the FD transfer plane for receiving FDs from root (e.g. session child FDs).
    let fd_transfer = stream.fd_transfer(sink.fd_sender());

    // Establish remoc connection over MuxSink/MuxStream.
    let (conn, mut base_tx, mut base_rx): (
        _,
        remoc::rch::base::Sender<WorkerHello>,
        remoc::rch::base::Receiver<WorkerBootstrap>,
    ) = remoc::Connect::framed(remoc::Cfg::default(), sink, stream)
        .await
        .whatever_context("failed to establish remoc transport")?;
    let worker_tasks = Arc::new(pishoo::hypervisor::task_scope::TaskScope::new());
    worker_tasks.spawn(|token| async move {
        tokio::select! {
            biased;
            () = token.cancelled() => {}
            result = conn.in_current_span() => {
                if let Err(error) = result {
                    tracing::debug!(
                        error = %Report::from_error(&error),
                        "root remoc connection ended"
                    );
                }
            }
        }
    });

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
    // Pass FdTransfer so worker-side requests can reserve receiver-chosen FD IDs.
    let plane = Arc::new(RemoteControlPlane::new(
        bootstrap.control_plane,
        fd_transfer,
    ));

    let dhttp_home = DhttpHome::new(bootstrap.home.join(".dhttp"));

    let mut term_signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .whatever_context("failed to create SIGTERM listener")?;
    let mut int_signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .whatever_context("failed to create SIGINT listener")?;
    let mut quit_signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::quit())
        .whatever_context("failed to create SIGQUIT listener")?;
    let mut hup_signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .whatever_context("failed to create SIGHUP listener")?;
    let mut usr1_signal =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())
            .whatever_context("failed to create SIGUSR1 listener")?;

    let router_state = gateway::reverse::router::RouterState {
        #[cfg(feature = "sshd")]
        session_spawner: plane.clone(),
        #[cfg(feature = "sshd")]
        task_scope: worker_tasks.clone(),
    };
    let mut runtime =
        pishoo::service::runtime::WorkerRuntime::new(plane.clone(), dhttp_home, router_state);
    runtime.start().await;

    loop {
        tokio::select! {
            _ = term_signal.recv() => {
                tracing::info!(signal = "SIGTERM", "received shutdown signal");
                break;
            }
            _ = int_signal.recv() => {
                tracing::info!(signal = "SIGINT", "received shutdown signal");
                break;
            }
            _ = quit_signal.recv() => {
                tracing::info!(signal = "SIGQUIT", "received shutdown signal");
                break;
            }
            _ = hup_signal.recv() => {
                tracing::info!(signal = "SIGHUP", "received reload signal");
                runtime.reload().await;
                tracing::info!(signal = "SIGHUP", "reload complete");
            }
            _ = usr1_signal.recv() => {
                tracing::info!(signal = "SIGUSR1", "received reopen signal");
            }
        }
    }

    runtime.shutdown().await;

    tracing::info!("exiting");
    worker_tasks.cancel_and_wait().await;
    Ok(())
}
