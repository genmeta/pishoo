//! Worker process handle: lifecycle management for spawned worker children.
//!
//! [`WorkerHandle`] wraps a raw Unix PID, provides non-blocking `try_wait`
//! and `start_kill`, and ensures the child is killed on drop.

use nix::{
    errno::Errno,
    sys::{
        signal::Signal,
        wait::{WaitPidFlag, WaitStatus, waitpid},
    },
    unistd::Pid,
};
use snafu::Snafu;

/// Handle to a spawned worker process.
///
/// Holds the child PID. Killing the child on drop ensures cleanup.
pub struct WorkerHandle {
    pid: Pid,
    exit_status: Option<WaitStatus>,
}

#[derive(Debug, Snafu)]
pub enum WorkerHandleError {
    #[snafu(display("failed to wait for worker process"))]
    Wait { source: Errno },
    #[snafu(display("failed to signal worker process"))]
    Signal { source: Errno },
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        // Best-effort kill — child may have already exited.
        let _ = self.start_kill();
        // Reap the child to avoid zombie processes. Non-blocking: if the
        // child hasn't exited yet (e.g. SIGKILL still pending), the kernel
        // will reparent it to init which reaps automatically.
        let _ = self.try_wait();
    }
}

impl WorkerHandle {
    pub(crate) fn from_pid(pid: Pid) -> Self {
        Self {
            pid,
            exit_status: None,
        }
    }

    pub fn pid(&self) -> Pid {
        self.pid
    }

    pub fn try_wait(&mut self) -> Result<Option<WaitStatus>, WorkerHandleError> {
        if let Some(status) = self.exit_status {
            return Ok(Some(status));
        }

        let status = match waitpid(self.pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => return Ok(None),
            Ok(status) => status,
            Err(source) => return Err(WorkerHandleError::Wait { source }),
        };

        self.exit_status = Some(status);
        Ok(Some(status))
    }

    pub fn start_kill(&mut self) -> Result<(), WorkerHandleError> {
        if self.exit_status.is_some() {
            return Ok(());
        }

        match nix::sys::signal::kill(self.pid, Signal::SIGKILL) {
            Ok(()) => Ok(()),
            Err(Errno::ESRCH) => Ok(()),
            Err(source) => Err(WorkerHandleError::Signal { source }),
        }
    }
}
