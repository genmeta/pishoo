//! Worker process spawning, monitoring, and signal forwarding.
//!
//! Spawns `pishoo-worker` binaries, establishes remoc IPC channels with the
//! new [`ControlPlane`](crate::ipc::ControlPlane) RTC trait, and
//! runs a monitor loop to detect and clean up exited workers.

use std::{path::PathBuf, sync::Arc};

use nix::unistd::{Gid, Pid, Uid};
use remoc::prelude::ServerShared;
use snafu::{OptionExt, ResultExt, Snafu};
use tracing::Instrument;

use crate::{
    ipc::{WorkerBootstrap, WorkerHello},
    root::{rpc_server::WorkerControlPlane, state::RootState},
    worker_spawn::WorkerHandle,
};

/// Result of successfully spawning a worker.
pub struct SpawnedWorker {
    pub handle: WorkerHandle,
    pub hello: WorkerHello,
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum SpawnWorkerError {
    #[snafu(display("failed to launch worker process"))]
    LaunchWorker {
        source: crate::launcher::LaunchWorkerError,
    },
    #[snafu(display("failed to establish remoc transport"))]
    ConnectTransport {
        source: remoc::ConnectError<std::io::Error, std::io::Error>,
    },
    #[snafu(display("failed to send worker bootstrap"))]
    SendBootstrap {
        source: remoc::rch::base::SendError<WorkerBootstrap>,
    },
    #[snafu(display("failed to receive worker hello"))]
    ReceiveHello { source: remoc::rch::base::RecvError },
    #[snafu(display("worker closed channel without sending startup hello"))]
    MissingHello,
}

/// Spawn a worker process with the new ControlPlane protocol.
///
/// 1. Fork + exec the pishoo-worker binary with privilege drop
/// 2. Establish remoc connection over stdin/stdout pipes
/// 3. Create a per-worker ControlPlane RPC server
/// 4. Send bootstrap (with ControlPlane client) → receive hello
pub async fn spawn_worker(
    worker_bin: &std::path::Path,
    uid: Uid,
    gid: Gid,
    username: String,
    home: PathBuf,
    state: Arc<RootState>,
) -> Result<SpawnedWorker, SpawnWorkerError> {
    let launched = crate::launcher::launch_worker(worker_bin, uid, gid, &username, &home)
        .context(spawn_worker_error::LaunchWorkerSnafu)?;
    let pid = launched.handle.pid().expect("child has pid");
    let transport = launched.transport;

    // Establish remoc connection over stdin/stdout pipes.
    let (conn, mut base_tx, mut base_rx): (
        _,
        remoc::rch::base::Sender<WorkerBootstrap>,
        remoc::rch::base::Receiver<WorkerHello>,
    ) = remoc::Connect::io(remoc::Cfg::default(), transport.stdout, transport.stdin)
        .await
        .context(spawn_worker_error::ConnectTransportSnafu)?;
    tokio::spawn(conn.in_current_span());

    // Create per-worker ControlPlane RPC server.
    let rpc_impl = WorkerControlPlane::new(Pid::from_raw(pid as i32), state.clone());

    // ControlPlane methods use &self, so ServerShared is appropriate.
    let (server, client) = crate::ipc::ControlPlaneServerShared::new(Arc::new(rpc_impl), 1);
    tokio::spawn(
        async move {
            let _ = server.serve(true).await;
        }
        .in_current_span(),
    );

    // Send bootstrap with new ControlPlane client.
    let bootstrap = WorkerBootstrap {
        uid: uid.as_raw(),
        username,
        home,
        control_plane: client,
    };

    base_tx
        .send(bootstrap)
        .await
        .context(spawn_worker_error::SendBootstrapSnafu)?;

    let hello = base_rx
        .recv()
        .await
        .context(spawn_worker_error::ReceiveHelloSnafu)?
        .context(spawn_worker_error::MissingHelloSnafu)?;

    Ok(SpawnedWorker {
        handle: launched.handle,
        hello,
    })
}

/// Run the worker monitor loop: poll workers for exit and clean up.
///
/// This task runs forever. It checks every 500ms whether any worker
/// processes have exited and cleans up their resources.
pub async fn run_monitor_loop(state: Arc<RootState>) {
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let exited = state.collect_exited_workers().await;
        for pid in exited {
            state.cleanup_worker_with_reason(pid, "child_exit").await;
        }
    }
}

/// Spawn the monitor loop as a background task. Returns the join handle.
pub fn spawn_monitor_loop(state: Arc<RootState>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run_monitor_loop(state).in_current_span())
}
