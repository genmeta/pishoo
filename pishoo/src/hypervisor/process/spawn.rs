//! Worker process spawn logic.

use std::{os::fd::OwnedFd, sync::Arc};

use h3x::ipc::transport::MuxChannel;
use remoc::prelude::ServerShared;
use snafu::{ResultExt, Snafu};
use tracing::Instrument;

use crate::{
    config::ResolvedWorkerTarget,
    hypervisor::{
        ipc_server::WorkerControlPlane,
        state::{RootState, WorkerProcessError, WorkerStartupError, worker_startup_error},
    },
    ipc::{WorkerBootstrap, WorkerHello},
};

pub const WORKER_STARTUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Result of successfully spawning a worker.
pub struct SpawnedWorker {
    pub pid: nix::unistd::Pid,
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum SpawnWorkerError {
    #[snafu(display("failed to launch worker process"))]
    LaunchWorker {
        source: crate::hypervisor::launcher::LaunchWorkerError,
    },
    #[snafu(display("failed to schedule worker startup task for pid {pid}"))]
    ScheduleStartup { pid: nix::unistd::Pid },
}

/// Spawn a worker process with the new MuxChannel protocol.
///
/// 1. Fork + exec the pishoo-worker binary with privilege drop
/// 2. Register the worker so background tasks can be tracked
/// 3. Schedule the startup handshake inside the worker task scope
pub async fn spawn_worker(
    worker_bin: &std::path::Path,
    target: &ResolvedWorkerTarget,
    state: Arc<RootState>,
) -> Result<SpawnedWorker, SpawnWorkerError> {
    let launched = crate::hypervisor::launcher::launch_worker(
        worker_bin,
        target.uid,
        target.gid,
        &target.name,
        &target.dir,
    )
    .context(spawn_worker_error::LaunchWorkerSnafu)?;
    let pid = launched.handle.pid();
    let mux_fd = launched.mux_fd;

    state
        .register_worker(pid, target.uid, target.name.clone(), launched.handle)
        .await;

    let uid = target.uid.as_raw();
    let username = target.name.clone();
    let home = target.dir.clone();
    let startup_state = state.clone();
    let spawned_startup_task = state
        .spawn_worker_task(pid, move |token| {
            let state = startup_state;
            async move {
                let result = tokio::select! {
                    () = token.cancelled() => return,
                    result = tokio::time::timeout(
                        WORKER_STARTUP_TIMEOUT,
                        start_worker_ipc(pid, state.clone(), mux_fd, uid, username, home),
                    ) => result,
                };
                let error = match result {
                    Ok(Ok(())) => return,
                    Ok(Err(source)) => WorkerProcessError::Startup { source },
                    Err(_) => WorkerProcessError::Startup {
                        source: WorkerStartupError::Timeout,
                    },
                };
                if !token.is_cancelled() {
                    state.fail_worker(pid, error).await;
                }
            }
            .in_current_span()
        })
        .await;
    if !spawned_startup_task {
        return Err(SpawnWorkerError::ScheduleStartup { pid });
    }

    tracing::info!(pid = %pid, username = %target.name, "worker launch scheduled");

    Ok(SpawnedWorker { pid })
}

async fn start_worker_ipc(
    pid: nix::unistd::Pid,
    state: Arc<RootState>,
    mux_fd: OwnedFd,
    uid: u32,
    username: String,
    home: std::path::PathBuf,
) -> Result<(), WorkerStartupError> {
    let mux = MuxChannel::from_fd(mux_fd).context(worker_startup_error::MuxChannelFromFdSnafu)?;
    let (sink, stream) = mux
        .split()
        .context(worker_startup_error::MuxChannelSplitSnafu)?;
    let fd_transfer = stream.fd_transfer(sink.fd_sender());
    let (conn, mut base_tx, mut base_rx): (
        _,
        remoc::rch::base::Sender<WorkerBootstrap>,
        remoc::rch::base::Receiver<WorkerHello>,
    ) = remoc::Connect::framed(remoc::Cfg::default(), sink, stream)
        .await
        .context(worker_startup_error::ConnectTransportSnafu)?;

    let connection_state = state.clone();
    let spawned_connection_task = state
        .spawn_worker_task(pid, move |token| {
            let state = connection_state;
            async move {
                tokio::select! {
                    () = token.cancelled() => {}
                    result = conn.in_current_span() => {
                        if let Err(error) = result {
                            tracing::debug!(
                                error = %snafu::Report::from_error(&error),
                                "worker remoc connection ended"
                            );
                        }
                        if !token.is_cancelled() {
                            let error = WorkerProcessError::IpcDisconnected;
                            state.fail_worker(pid, error).await;
                        }
                    }
                }
            }
        })
        .await;
    if !spawned_connection_task {
        return Err(WorkerStartupError::WorkerUnregistered);
    }

    let rpc_impl = WorkerControlPlane::new(pid, state.clone(), fd_transfer);
    let (server, client) = crate::ipc::ControlPlaneServerShared::new(Arc::new(rpc_impl), 1);
    let username_for_log = username.clone();
    let bootstrap = WorkerBootstrap {
        uid,
        username,
        home,
        control_plane: client,
    };
    let server_state = state.clone();
    let spawned_server_task = state
        .spawn_worker_task(pid, move |token| {
            let state = server_state;
            async move {
                tokio::select! {
                    () = token.cancelled() => {}
                    result = server.serve(true) => {
                        if let Err(error) = result {
                            tracing::debug!(
                                error = %snafu::Report::from_error(&error),
                                "worker control plane server ended"
                            );
                        }
                        if !token.is_cancelled() {
                            state
                                .fail_worker(pid, WorkerProcessError::IpcDisconnected)
                                .await;
                        }
                    }
                }
            }
            .in_current_span()
        })
        .await;
    if !spawned_server_task {
        return Err(WorkerStartupError::WorkerUnregistered);
    }

    base_tx
        .send(bootstrap)
        .await
        .context(worker_startup_error::SendBootstrapSnafu)?;
    let hello = match base_rx.recv().await {
        Ok(Some(hello)) => hello,
        Ok(None) => return Err(WorkerStartupError::MissingHello),
        Err(source) => return Err(WorkerStartupError::ReceiveHello { source }),
    };
    tracing::info!(pid = %pid, hello_pid = hello.pid, username = %username_for_log, "worker started");
    Ok(())
}
