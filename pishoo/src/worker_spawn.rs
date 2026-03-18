//! Root-side worker process spawning.
//!
//! Spawns the `pishoo-worker` binary as a target user, establishes a remoc
//! connection over stdin/stdout pipes, and sends [`WorkerBootstrap`] to the child.
//!
//! # Protocol
//!
//! 1. Root spawns `pishoo-worker` with stdin/stdout piped, stderr inherited.
//! 2. `pre_exec` drops privileges: `setgid` → `initgroups` → `setuid`.
//! 3. Root establishes remoc connection: reads from child's stdout, writes to child's stdin.
//! 4. Root sends [`WorkerBootstrap`] via base channel.
//! 5. Child receives bootstrap and sends `()` ack back.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use nix::unistd::{Pid, Uid};
use remoc::rtc::ServerShared;
use tokio::process::Child;
use std::process::ExitStatus;
use tokio::sync::Mutex;

use crate::protocol::{RootTransportApiServerShared, WorkerBootstrap, WorkerHello};

/// Handle to a spawned worker process.
///
/// Holds the child process. Killing the child on drop ensures cleanup.
pub struct WorkerHandle {
    child: Child,
}

pub struct SpawnedWorker {
    pub handle: WorkerHandle,
    pub hello: WorkerHello,
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        // Best-effort kill — child may have already exited.
        let _ = self.start_kill();
    }
}

impl WorkerHandle {
    pub fn new(child: Child) -> Self {
        Self { child }
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>, std::io::Error> {
        self.child.try_wait()
    }

    pub fn start_kill(&mut self) -> Result<(), std::io::Error> {
        self.child.start_kill()
    }
}

/// Spawn a `pishoo-worker` process for the given user and establish remoc channels.
///
/// # Arguments
///
/// * `worker_bin` — Path to the `pishoo-worker` binary.
/// * `uid` — Target user's UID (for privilege drop).
/// * `gid` — Target user's primary GID (for privilege drop).
/// * `username` — Target username (for `initgroups` and bootstrap).
/// * `home` — Target user's home directory.
/// * `log_dir` — Directory where the worker should write logs.
/// * `state` — Shared root state for creating per-worker transport API.
/// # Errors
///
/// Returns an error if the process cannot be spawned or the remoc handshake fails.
pub async fn spawn_worker(
    worker_bin: impl AsRef<Path>,
    uid: Uid,
    gid: u32,
    username: String,
    home: PathBuf,
    log_dir: PathBuf,
    state: Arc<Mutex<crate::root_state::RootState>>,
) -> Result<SpawnedWorker, std::io::Error> {
    let launched = crate::launcher::launch_worker(worker_bin.as_ref(), uid, gid, &username)?;
    let pid = launched.handle.pid().expect("child has pid");
    let transport = launched.transport;

    let (conn, mut base_tx, mut base_rx): (
        _,
        remoc::rch::base::Sender<WorkerBootstrap>,
        remoc::rch::base::Receiver<WorkerHello>,
    ) = remoc::Connect::io(remoc::Cfg::default(), transport.stdout, transport.stdin)
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::ConnectionRefused, e))?;
    tokio::spawn(conn);
    let (server, client) = RootTransportApiServerShared::new(
        Arc::new(crate::root_transport_api::RootTransportApiImpl::new(
            Pid::from_raw(pid as i32),
            state.clone(),
        )),
        1,
    );
    tokio::spawn(async move { server.serve(true).await });

    let bootstrap = WorkerBootstrap {
        uid: uid.as_raw(),
        username,
        home,
        log_dir,
        root_api: client,
    };

    base_tx
        .send(bootstrap)
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::BrokenPipe, e.to_string()))?;

    let hello = base_rx
        .recv()
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::BrokenPipe, e.to_string()))?
        .ok_or_else(|| std::io::Error::other("worker closed channel without sending startup hello"))?;

    Ok(SpawnedWorker {
        handle: launched.handle,
        hello,
    })
}
