//! Root-side worker process spawning.
//!
//! Spawns the `pishoo-worker` binary as a target user, establishes a remoc
//! connection over stdin/stdout pipes, and sends [`WorkerBootstrap`] to the child.
//!
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use nix::{
    errno::Errno,
    libc,
    sys::{
        signal::Signal,
        wait::{WaitPidFlag, WaitStatus, waitpid},
    },
    unistd::{Pid, Uid},
};
use remoc::rtc::ServerShared;
use snafu::{OptionExt, ResultExt, Snafu};
use tokio::{process::Child, sync::Mutex};
use tracing::Instrument;

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
    exit_status: Option<WaitStatus>,
}

pub struct SpawnedWorker {
    pub handle: WorkerHandle,
    pub hello: WorkerHello,
}

#[derive(Debug, Snafu)]
pub enum WorkerHandleError {
    #[snafu(display("failed to wait for worker process"))]
    Wait { source: Errno },
    #[snafu(display("failed to signal worker process"))]
    Signal { source: Errno },
}

#[derive(Debug, Snafu)]
pub enum SpawnWorkerError {
    #[snafu(display("failed to launch worker process"))]
    LaunchWorker { source: crate::launcher::LaunchWorkerError },
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

    pub fn try_wait(&mut self) -> Result<Option<WaitStatus>, WorkerHandleError> {
        match &mut self.inner {
            WorkerHandleInner::Tokio(child) => child
                .try_wait()
                .map_err(|source| WorkerHandleError::Wait {
                    source: Errno::from_raw(source.raw_os_error().unwrap_or(libc::EIO)),
                })
                .map(|status| {
                    status.map(|s| {
                        let pid = child
                            .id()
                            .map(|id| Pid::from_raw(id as i32))
                            .unwrap_or_else(|| Pid::from_raw(0));
                        WaitStatus::Exited(pid, s.code().unwrap_or_default())
                    })
                }),
            WorkerHandleInner::Unix(child) => child.try_wait(),
        }
    }

    pub fn start_kill(&mut self) -> Result<(), WorkerHandleError> {
        match &mut self.inner {
            WorkerHandleInner::Tokio(child) => child.start_kill().map_err(|source| {
                WorkerHandleError::Signal {
                    source: Errno::from_raw(source.raw_os_error().unwrap_or(libc::EIO)),
                }
            }),
            WorkerHandleInner::Unix(child) => child.start_kill(),
        }
    }
}

impl UnixWorkerHandle {
    fn try_wait(&mut self) -> Result<Option<WaitStatus>, WorkerHandleError> {
        if let Some(status) = self.exit_status {
            return Ok(Some(status));
        }

        let status = match waitpid(Pid::from_raw(self.pid as i32), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => return Ok(None),
            Ok(status) => status,
            Err(source) => return Err(WorkerHandleError::Wait { source }),
        };

        self.exit_status = Some(status);
        Ok(Some(status))
    }

    fn start_kill(&mut self) -> Result<(), WorkerHandleError> {
        if self.exit_status.is_some() {
            return Ok(());
        }

        match nix::sys::signal::kill(Pid::from_raw(self.pid as i32), Signal::SIGKILL) {
            Ok(()) => Ok(()),
            Err(nix::errno::Errno::ESRCH) => Ok(()),
            Err(source) => Err(WorkerHandleError::Signal { source }),
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
/// * `username` — Target username.
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
) -> Result<SpawnedWorker, SpawnWorkerError> {
    let launched = crate::launcher::launch_worker(worker_bin.as_ref(), uid, gid, &username, &home)
        .context(LaunchWorkerSnafu)?;
    let pid = launched.handle.pid().expect("child has pid");
    let transport = launched.transport;

    let (conn, mut base_tx, mut base_rx): (
        _,
        remoc::rch::base::Sender<WorkerBootstrap>,
        remoc::rch::base::Receiver<WorkerHello>,
    ) = remoc::Connect::io(remoc::Cfg::default(), transport.stdout, transport.stdin)
        .await
        .context(ConnectTransportSnafu)?;
    tokio::spawn(conn.in_current_span());
    let (server, client) = RootTransportApiServerShared::new(
        Arc::new(crate::root_transport_api::RootTransportApiImpl::new(
            Pid::from_raw(pid as i32),
            state.clone(),
        )),
        1,
    );
    tokio::spawn(async move { server.serve(true).await }.in_current_span());

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
        .context(SendBootstrapSnafu)?;

    let hello = base_rx
        .recv()
        .await
        .context(ReceiveHelloSnafu)?
        .context(MissingHelloSnafu)?;

    Ok(SpawnedWorker {
        handle: launched.handle,
        hello,
    })
}
