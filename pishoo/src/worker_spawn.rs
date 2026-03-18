//! Root-side worker process spawning.
//!
//! Spawns the `pishoo-worker` binary as a target user, establishes a remoc
//! connection over stdin/stdout pipes, and sends [`WorkerBootstrap`] to the child.
//!
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{os::unix::process::ExitStatusExt, process::ExitStatus};

use nix::{sys::signal::Signal, unistd::Pid};
use nix::unistd::Uid;
use remoc::rtc::ServerShared;
use tokio::process::Child;
use tokio::sync::Mutex;

use crate::protocol::{RootTransportApiServerShared, WorkerBootstrap, WorkerHello};

/// Handle to a spawned worker process.
///
/// Holds the child process. Killing the child on drop ensures cleanup.
pub struct WorkerHandle {
    inner: WorkerHandleInner,
}

enum WorkerHandleInner {
    Tokio(Child),
    Unix(UnixWorkerHandle),
}

struct UnixWorkerHandle {
    pid: u32,
    exit_status: Option<ExitStatus>,
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
        Self {
            inner: WorkerHandleInner::Tokio(child),
        }
    }

    pub(crate) fn from_unix_pid(pid: u32) -> Self {
        Self {
            inner: WorkerHandleInner::Unix(UnixWorkerHandle {
                pid,
                exit_status: None,
            }),
        }
    }

    pub fn pid(&self) -> Option<u32> {
        match &self.inner {
            WorkerHandleInner::Tokio(child) => child.id(),
            WorkerHandleInner::Unix(child) => Some(child.pid),
        }
    }

    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>, std::io::Error> {
        match &mut self.inner {
            WorkerHandleInner::Tokio(child) => child.try_wait(),
            WorkerHandleInner::Unix(child) => child.try_wait(),
        }
    }

    pub fn start_kill(&mut self) -> Result<(), std::io::Error> {
        match &mut self.inner {
            WorkerHandleInner::Tokio(child) => child.start_kill(),
            WorkerHandleInner::Unix(child) => child.start_kill(),
        }
    }
}

impl UnixWorkerHandle {
    fn try_wait(&mut self) -> Result<Option<ExitStatus>, std::io::Error> {
        if let Some(status) = self.exit_status {
            return Ok(Some(status));
        }

        let mut raw_status = 0;
        let waited = unsafe { libc::waitpid(self.pid as libc::pid_t, &mut raw_status, libc::WNOHANG) };
        if waited == 0 {
            return Ok(None);
        }
        if waited == -1 {
            return Err(std::io::Error::last_os_error());
        }

        let status = ExitStatus::from_raw(raw_status);
        self.exit_status = Some(status);
        Ok(Some(status))
    }

    fn start_kill(&mut self) -> Result<(), std::io::Error> {
        if self.exit_status.is_some() {
            return Ok(());
        }

        match nix::sys::signal::kill(Pid::from_raw(self.pid as i32), Signal::SIGKILL) {
            Ok(()) => Ok(()),
            Err(nix::errno::Errno::ESRCH) => Ok(()),
            Err(err) => Err(std::io::Error::from_raw_os_error(err as i32)),
        }
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
    let launched = crate::launcher::launch_worker(worker_bin.as_ref(), uid, gid, &username, &home)?;
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
