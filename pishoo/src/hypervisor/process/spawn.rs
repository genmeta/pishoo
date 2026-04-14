//! Worker process spawn logic.

use std::sync::Arc;

use remoc::prelude::ServerShared;
use snafu::{ResultExt, Snafu};
use tracing::Instrument;

use crate::{
    config::ResolvedWorkerTarget,
    hypervisor::{rpc_server::WorkerControlPlane, state::RootState},
    ipc::{WorkerBootstrap, WorkerHello},
};

/// Result of successfully spawning a worker.
pub struct SpawnedWorker {
    pub hello: WorkerHello,
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum SpawnWorkerError {
    #[snafu(display("failed to launch worker process"))]
    LaunchWorker {
        source: crate::hypervisor::launcher::LaunchWorkerError,
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
/// 3. Register the worker early so background tasks can be tracked
/// 4. Create a per-worker ControlPlane RPC server
/// 5. Send bootstrap (with ControlPlane client) → receive hello
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
    let transport = launched.transport;

    // Establish remoc connection over stdin/stdout pipes.
    let (conn, mut base_tx, mut base_rx): (
        _,
        remoc::rch::base::Sender<WorkerBootstrap>,
        remoc::rch::base::Receiver<WorkerHello>,
    ) = remoc::Connect::io(remoc::Cfg::default(), transport.stdout, transport.stdin)
        .await
        .context(spawn_worker_error::ConnectTransportSnafu)?;

    // Register the worker now so that spawn_worker_task can track tasks.
    // On any subsequent failure, cleanup_worker_with_reason undoes this.
    state
        .register_worker(pid, target.uid, target.name.clone(), launched.handle)
        .await;

    state
        .spawn_worker_task(pid, async move {
            let _ = conn.in_current_span().await;
        })
        .await;

    // Create per-worker ControlPlane RPC server.
    let rpc_impl = WorkerControlPlane::new(
        pid,
        state.clone(),
        #[cfg(feature = "sshd")]
        launched.seqpacket,
    );

    // ControlPlane methods use &self, so ServerShared is appropriate.
    let (server, client) = crate::ipc::ControlPlaneServerShared::new(Arc::new(rpc_impl), 1);
    state
        .spawn_worker_task(
            pid,
            async move {
                let _ = server.serve(true).await;
            }
            .in_current_span(),
        )
        .await;

    // Send bootstrap with new ControlPlane client.
    let bootstrap = WorkerBootstrap {
        uid: target.uid.as_raw(),
        username: target.name.clone(),
        home: target.dir.clone(),
        control_plane: client,
    };

    if let Err(source) = base_tx.send(bootstrap).await {
        state
            .cleanup_worker_with_reason(pid, "send_bootstrap_failed")
            .await;
        return Err(SpawnWorkerError::SendBootstrap { source });
    }

    let hello = match base_rx.recv().await {
        Ok(Some(hello)) => hello,
        Ok(None) => {
            state.cleanup_worker_with_reason(pid, "missing_hello").await;
            return Err(SpawnWorkerError::MissingHello);
        }
        Err(source) => {
            state
                .cleanup_worker_with_reason(pid, "receive_hello_failed")
                .await;
            return Err(SpawnWorkerError::ReceiveHello { source });
        }
    };

    tracing::info!(pid = %pid, username = %target.name, "worker spawned");

    Ok(SpawnedWorker { hello })
}
