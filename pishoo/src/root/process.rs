//! Worker process spawning, monitoring, and signal forwarding.
//!
//! Spawns `pishoo-worker` binaries, establishes remoc IPC channels with the
//! new [`ControlPlane`](crate::ipc::ControlPlane) RTC trait, and
//! runs a monitor loop to detect and clean up exited workers.

use std::{path::PathBuf, sync::Arc};

use nix::unistd::{Gid, Uid};
use remoc::prelude::ServerShared;
use snafu::{Report, ResultExt, Snafu};
use tokio_util::task::AbortOnDropHandle;
use tracing::Instrument;

use crate::{
    ipc::{WorkerBootstrap, WorkerHello},
    root::{rpc_server::WorkerControlPlane, state::RootState, worker_handle::WorkerHandle},
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
        source: crate::root::launcher::LaunchWorkerError,
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
    let launched = crate::root::launcher::launch_worker(worker_bin, uid, gid, &username, &home)
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
        uid: uid.as_raw(),
        username,
        home,
        control_plane: client,
    };

    if let Err(source) = base_tx.send(bootstrap).await {
        state.cleanup_worker_tasks(pid).await;
        return Err(SpawnWorkerError::SendBootstrap { source });
    }

    let hello = match base_rx.recv().await {
        Ok(Some(hello)) => hello,
        Ok(None) => {
            state.cleanup_worker_tasks(pid).await;
            return Err(SpawnWorkerError::MissingHello);
        }
        Err(source) => {
            state.cleanup_worker_tasks(pid).await;
            return Err(SpawnWorkerError::ReceiveHello { source });
        }
    };

    Ok(SpawnedWorker {
        handle: launched.handle,
        hello,
    })
}

/// Run the worker monitor loop: wait for SIGCHLD notification or 5s fallback,
/// then reap all exited workers.
///
/// SIGCHLD signals may be coalesced by the kernel, so we loop `collect_exited_workers`
/// until no more exits are found after each wake-up.
pub async fn run_monitor_loop(state: Arc<RootState>) {
    loop {
        tokio::select! {
            _ = state.worker_notify.notified() => {}
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
        }
        // SIGCHLD coalescing: loop waitpid until no more exited children.
        loop {
            let exited = state.collect_exited_workers().await;
            if exited.is_empty() {
                break;
            }
            for pid in exited {
                state.cleanup_worker_with_reason(pid, "child_exit").await;
            }
        }
    }
}

/// Spawn the monitor loop as a background task. Returns the join handle.
pub fn spawn_monitor_loop(state: Arc<RootState>) -> AbortOnDropHandle<()> {
    AbortOnDropHandle::new(tokio::spawn(run_monitor_loop(state).in_current_span()))
}

// ---------------------------------------------------------------------------
// Worker binary resolution and batch spawning
// ---------------------------------------------------------------------------

/// Resolve the path of the `pishoo-worker` binary.
///
/// Search order:
/// 1. Runtime env var `PISHOO_WORKER_BIN`
/// 2. Compile-time env var `PISHOO_WORKER_BIN` (set by deb builds)
/// 3. `<exe_dir>/../libexec/pishoo-worker` (Homebrew layout)
/// 4. `<exe_dir>/pishoo-worker` (debug / same-dir fallback)
pub(crate) fn worker_binary_path() -> PathBuf {
    // 1. Runtime environment variable
    if let Ok(path) = std::env::var("PISHOO_WORKER_BIN") {
        return PathBuf::from(path);
    }

    // 2. Compile-time environment variable (set during release deb builds)
    if let Some(path) = option_env!("PISHOO_WORKER_BIN") {
        return PathBuf::from(path);
    }

    if let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    {
        // 3. Homebrew libexec layout: <prefix>/bin/pishoo → <prefix>/libexec/pishoo-worker
        let libexec = exe_dir.join("../libexec/pishoo-worker");
        if libexec.exists() {
            return libexec;
        }

        // 4. Same directory (debug builds, Windows, cargo build output)
        return exe_dir.join("pishoo-worker");
    }

    PathBuf::from("pishoo-worker")
}

/// Spawn worker processes for all resolved targets.
///
/// Failures are per-user and logged; they do not prevent other workers
/// from being spawned.
pub async fn spawn_configured_workers(
    state: &Arc<RootState>,
    worker_targets: Vec<crate::config::ResolvedWorkerTarget>,
) {
    if worker_targets.is_empty() {
        return;
    }

    let worker_bin = worker_binary_path();

    for target in worker_targets {
        let spawned = match spawn_worker(
            &worker_bin,
            target.uid,
            target.gid,
            target.name.clone(),
            target.dir.clone(),
            state.clone(),
        )
        .await
        {
            Ok(spawned) => spawned,
            Err(error) => {
                // Worker spawn failures (fork/exec, IPC negotiation, hello
                // timeout) are per-user problems that must not bring down the
                // entire root process.  Log and continue to the next worker.
                tracing::error!(
                    user = %target.name,
                    error = %Report::from_error(&error),
                    "failed to spawn worker, skipping user"
                );
                continue;
            }
        };
        let pid = spawned.handle.pid();

        state.register_worker(pid, target.uid, spawned.handle).await;
    }
}
