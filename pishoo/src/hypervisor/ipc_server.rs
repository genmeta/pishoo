//! ControlPlane implementation for the root process (remoc RPC server side).
//!
//! Each worker process gets its own [`WorkerControlPlane`] instance, bound to
//! the worker's PID. When a worker calls `listen()` or `connect()`, this
//! module creates the actual QUIC resources and returns IPC capability handles.

use std::sync::Arc;
#[cfg(feature = "sshd")]
use std::time::Duration;

#[cfg(feature = "sshd")]
use dhttp::h3x::ipc::transport::FdVec;
use dhttp::h3x::ipc::{
    quic::{
        ConnectAdapter, IpcConnectClient, IpcConnectServerShared, IpcListenServerSharedMut,
        ListenAdapter,
    },
    transport::FdTransfer,
};
use gateway::control_plane::{ConnectorRequest, ListenRequest};
use nix::unistd::Pid;
#[cfg(feature = "sshd")]
use nix::{
    errno::Errno,
    sys::{
        signal::{Signal, kill},
        wait::{WaitPidFlag, WaitStatus, waitpid},
    },
};
use remoc::prelude::{ServerShared, ServerSharedMut};
#[cfg(feature = "sshd")]
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use super::{endpoint_factory, state::RootState, task_scope::TaskScope};
use crate::{
    hypervisor::state::{AcquireListenerError, RebuildListenerError},
    ipc::{ConnectError, ListenError, RebuildListenError},
};

/// Per-worker [`ControlPlane`](crate::ipc::ControlPlane) implementation.
///
/// Created for each worker process with a fixed `caller_pid`. Delegates
/// resource creation to the shared QUIC infrastructure and tracks ownership
/// in the root state.
pub struct WorkerControlPlane {
    caller_pid: Pid,
    state: Arc<RootState>,
    /// FD transfer plane from the root-side MuxChannel.
    fd_transfer: FdTransfer,
}

impl WorkerControlPlane {
    pub fn new(caller_pid: Pid, state: Arc<RootState>, fd_transfer: FdTransfer) -> Self {
        Self {
            caller_pid,
            state,
            fd_transfer,
        }
    }

    fn wrap_listener(
        &self,
        task_scope: TaskScope,
        server_name: String,
        adapter: crate::listen::RegisteredEndpoint,
    ) -> dhttp::h3x::ipc::quic::IpcListenClient {
        let listen_adapter = ListenAdapter::<_, IpcCodec>::new(adapter, self.fd_transfer.clone());
        let (server, client) =
            IpcListenServerSharedMut::new(Arc::new(tokio::sync::RwLock::new(listen_adapter)), 64);
        task_scope.spawn(|token| {
            async move {
                tokio::select! {
                    biased;
                    () = token.cancelled() => {}
                    _ = server.serve(true) => {}
                }
            }
            .in_current_span()
        });

        tracing::debug!(
            caller_pid = %self.caller_pid,
            %server_name,
            "listener request fulfilled (IPC)"
        );
        client
    }
}

/// The remoc codec used for per-connection MuxChannel remoc links.
///
/// Must match the codec used by the worker-side [`IpcListener`] /
/// [`IpcConnector`].
type IpcCodec = remoc::codec::Default;

impl crate::ipc::ControlPlane for WorkerControlPlane {
    async fn listener(
        &self,
        request: ListenRequest,
    ) -> Result<dhttp::h3x::ipc::quic::IpcListenClient, ListenError> {
        let server_name = request.identity.name().as_full().to_owned();
        let owner = self
            .state
            .owner_for_pid(self.caller_pid)
            .await
            .ok_or_else(|| ListenError::Internal {
                message: format!("unknown caller pid {}", self.caller_pid),
            })?;
        let task_scope = self
            .state
            .task_scope_for_owner(owner)
            .await
            .ok_or_else(|| ListenError::Internal {
                message: format!("unknown caller pid {}", self.caller_pid),
            })?;

        let adapter = self
            .state
            .acquire_listener(owner, request)
            .await
            .map_err(|error| {
                let report = snafu::Report::from_error(&error).to_string();
                tracing::warn!(
                    caller_pid = %self.caller_pid,
                    %server_name,
                    error = %report,
                    "listen request failed"
                );
                match error {
                    AcquireListenerError::DuplicateListen
                    | AcquireListenerError::ConflictedName => ListenError::Conflict,
                    AcquireListenerError::BuildBindPatterns { .. } => {
                        ListenError::InvalidRequest { reason: report }
                    }
                    AcquireListenerError::BuildResolver { .. }
                    | AcquireListenerError::BuildEndpoint { .. }
                    | AcquireListenerError::CreatePublisher { .. }
                    | AcquireListenerError::MissingPublisher
                    | AcquireListenerError::OwnerUnavailable
                    | AcquireListenerError::TransitionStopped => ListenError::Internal {
                        message: format!("failed to prepare endpoint for `{server_name}`"),
                    },
                }
            })?;

        Ok(self.wrap_listener(task_scope, server_name.clone(), adapter))
    }

    async fn rebuild_listener(
        &self,
        request: ListenRequest,
    ) -> Result<dhttp::h3x::ipc::quic::IpcListenClient, RebuildListenError> {
        let server_name = request.identity.name().as_full().to_owned();
        let owner = self
            .state
            .owner_for_pid(self.caller_pid)
            .await
            .ok_or_else(|| RebuildListenError::Internal {
                message: format!("unknown caller pid {}", self.caller_pid),
            })?;
        let task_scope = self
            .state
            .task_scope_for_owner(owner)
            .await
            .ok_or_else(|| RebuildListenError::Internal {
                message: format!("unknown caller pid {}", self.caller_pid),
            })?;

        let adapter = self
            .state
            .rebuild_listener(owner, request)
            .await
            .map_err(|error| {
                let report = snafu::Report::from_error(&error).to_string();
                tracing::warn!(
                    caller_pid = %self.caller_pid,
                    %server_name,
                    error = %report,
                    "rebuild listener request failed"
                );
                match error {
                    RebuildListenerError::NotOwner => RebuildListenError::NotOwner,
                    RebuildListenerError::ConflictedName => RebuildListenError::Conflict,
                    RebuildListenerError::TransitionStopped => RebuildListenError::Internal {
                        message: format!("failed to replace endpoint for `{server_name}`"),
                    },
                    RebuildListenerError::Replacement { source } => match source {
                        AcquireListenerError::BuildBindPatterns { .. } => {
                            RebuildListenError::InvalidRequest { reason: report }
                        }
                        AcquireListenerError::DuplicateListen
                        | AcquireListenerError::ConflictedName => RebuildListenError::Conflict,
                        AcquireListenerError::BuildResolver { .. }
                        | AcquireListenerError::BuildEndpoint { .. }
                        | AcquireListenerError::CreatePublisher { .. }
                        | AcquireListenerError::MissingPublisher
                        | AcquireListenerError::OwnerUnavailable
                        | AcquireListenerError::TransitionStopped => {
                            RebuildListenError::Replacement { reason: report }
                        }
                    },
                }
            })?;

        Ok(self.wrap_listener(task_scope, server_name.clone(), adapter))
    }

    async fn connector(&self, request: ConnectorRequest) -> Result<IpcConnectClient, ConnectError> {
        // Verify caller is a registered worker.
        let owner = self
            .state
            .owner_for_pid(self.caller_pid)
            .await
            .ok_or_else(|| ConnectError::Internal {
                message: format!("unknown caller pid {}", self.caller_pid),
            })?;
        let task_scope = self
            .state
            .task_scope_for_owner(owner)
            .await
            .ok_or_else(|| ConnectError::Internal {
                message: format!("unknown caller pid {}", self.caller_pid),
            })?;

        // Build a DHTTP endpoint with the requested client identity (if any).
        let endpoint = endpoint_factory::build_connector_endpoint(
            self.state.network.clone(),
            request.identity,
        )
        .await;
        let endpoint = Arc::new(match endpoint {
            Ok(endpoint) => endpoint,
            Err(error) => {
                return Err(ConnectError::Internal {
                    message: format!(
                        "failed to build connector endpoint: {}",
                        snafu::Report::from_error(&error)
                    ),
                });
            }
        });

        // Wrap the endpoint in ConnectAdapter for IPC capability forwarding.
        let connect_adapter =
            ConnectAdapter::<_, IpcCodec>::new(endpoint, self.fd_transfer.clone());
        let (server, client) = IpcConnectServerShared::new(Arc::new(connect_adapter), 64);
        task_scope.spawn(|token| {
            async move {
                tokio::select! {
                    biased;
                    () = token.cancelled() => {}
                    _ = server.serve(true) => {}
                }
            }
            .in_current_span()
        });

        tracing::debug!(
            caller_pid = %self.caller_pid,
            "connect request fulfilled (IPC)"
        );
        Ok(client)
    }

    async fn spawn_session(
        &self,
        username: String,
        fd_id_raw: u64,
    ) -> Result<u64, crate::ipc::SpawnSessionError> {
        #[cfg(feature = "sshd")]
        {
            tracing::info!(
                caller_pid = %self.caller_pid,
                %username,
                fd_id = fd_id_raw,
                "spawn session request received"
            );

            let fd_id = dhttp::h3x::varint::VarInt::try_from(fd_id_raw).map_err(|error| {
                crate::ipc::SpawnSessionError::SpawnFailed {
                    reason: snafu::Report::from_error(&error).to_string(),
                }
            })?;
            let delivery = self.fd_transfer.delivery(fd_id);

            // Fork the session process as root (no privilege drop).
            let transport =
                crate::hypervisor::launcher::launch_session(&username).map_err(|e| {
                    crate::ipc::SpawnSessionError::SpawnFailed {
                        reason: snafu::Report::from_error(e).to_string(),
                    }
                })?;

            let child = SessionChildGuard::new(transport.child_pid);
            let mut fds = FdVec::new();
            fds.push(transport.mux_fd);
            if let Err(error) = delivery.deliver(fds).await {
                child.terminate().await;
                return Err(crate::ipc::SpawnSessionError::SpawnFailed {
                    reason: snafu::Report::from_error(&error).to_string(),
                });
            }

            // Reap the session child process to avoid zombies. The reaper is
            // scoped to the worker so worker cleanup can cancel it and then
            // wait for the session child to be terminated and reaped.
            let state = self.state.clone();
            let caller_pid = self.caller_pid;
            let child_pid = child.pid();
            let spawned_reaper = state
                .spawn_worker_task(caller_pid, move |token| async move {
                    reap_session_child(child_pid, token).await;
                })
                .await;
            if !spawned_reaper {
                child.terminate().await;
                return Err(crate::ipc::SpawnSessionError::SpawnFailed {
                    reason: format!(
                        "worker {caller_pid} exited before session reaper was registered"
                    ),
                });
            }
            child.disarm();

            tracing::info!(
                caller_pid = %self.caller_pid,
                %username,
                fd_id = %fd_id,
                "session spawned, FD queued to worker"
            );
            Ok(u64::from(fd_id))
        }
        #[cfg(not(feature = "sshd"))]
        {
            let _ = (username, fd_id_raw);
            Err(crate::ipc::SpawnSessionError::NotSupported)
        }
    }
}

#[cfg(feature = "sshd")]
struct SessionChildGuard {
    child_pid: Pid,
    armed: bool,
}

#[cfg(feature = "sshd")]
impl SessionChildGuard {
    fn new(child_pid: Pid) -> Self {
        Self {
            child_pid,
            armed: true,
        }
    }

    fn pid(&self) -> Pid {
        self.child_pid
    }

    fn disarm(mut self) -> Pid {
        self.armed = false;
        self.child_pid
    }

    async fn terminate(mut self) {
        let child_pid = self.child_pid;
        self.armed = false;
        terminate_session_child(child_pid).await;
    }
}

#[cfg(feature = "sshd")]
impl Drop for SessionChildGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }

        tracing::warn!(
            child_pid = %self.child_pid,
            "session child ownership handoff cancelled before reaper registration"
        );
        send_session_signal(self.child_pid, Signal::SIGKILL);
        let _ = poll_session_child(self.child_pid);
    }
}

#[cfg(feature = "sshd")]
async fn reap_session_child(child_pid: Pid, token: CancellationToken) {
    let poll_interval = Duration::from_millis(50);

    loop {
        if poll_session_child(child_pid) {
            return;
        }

        tokio::select! {
            biased;
            () = token.cancelled() => {
                terminate_session_child(child_pid).await;
                return;
            }
            () = tokio::time::sleep(poll_interval) => {}
        }
    }
}

#[cfg(feature = "sshd")]
async fn terminate_session_child(child_pid: Pid) {
    const TERMINATE_GRACE: Duration = Duration::from_secs(2);

    if poll_session_child(child_pid) {
        return;
    }

    if wait_session_child_for(child_pid, TERMINATE_GRACE).await {
        return;
    }

    send_session_signal(child_pid, Signal::SIGTERM);
    if wait_session_child_for(child_pid, TERMINATE_GRACE).await {
        return;
    }

    tracing::warn!(
        child_pid = %child_pid,
        "session child did not exit after SIGTERM"
    );
    send_session_signal(child_pid, Signal::SIGKILL);
    if !wait_session_child_for(child_pid, TERMINATE_GRACE).await {
        tracing::warn!(
            child_pid = %child_pid,
            "session child did not reap after SIGKILL"
        );
    }
}

#[cfg(feature = "sshd")]
async fn wait_session_child_for(child_pid: Pid, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if poll_session_child(child_pid) {
            return true;
        }

        if tokio::time::Instant::now() >= deadline {
            return false;
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(feature = "sshd")]
fn poll_session_child(child_pid: Pid) -> bool {
    match waitpid(child_pid, Some(WaitPidFlag::WNOHANG)) {
        Ok(WaitStatus::StillAlive) => false,
        Ok(status) => {
            tracing::debug!(child_pid = %child_pid, ?status, "session child exited");
            true
        }
        Err(Errno::ECHILD) => true,
        Err(error) => {
            tracing::warn!(
                child_pid = %child_pid,
                error = %snafu::Report::from_error(&error),
                "failed to wait for session child"
            );
            true
        }
    }
}

#[cfg(feature = "sshd")]
fn send_session_signal(child_pid: Pid, signal: Signal) {
    match kill(child_pid, signal) {
        Ok(()) => {
            tracing::debug!(child_pid = %child_pid, ?signal, "sent signal to session child");
        }
        Err(Errno::ESRCH) => {}
        Err(error) => {
            tracing::warn!(
                child_pid = %child_pid,
                ?signal,
                error = %snafu::Report::from_error(&error),
                "failed to signal session child"
            );
        }
    }
}

#[cfg(all(test, feature = "sshd"))]
mod tests {
    use nix::{
        errno::Errno,
        sys::wait::{WaitPidFlag, waitpid},
        unistd::Pid,
    };
    use tokio_util::sync::CancellationToken;

    use super::reap_session_child;

    #[tokio::test]
    async fn session_reaper_terminates_and_reaps_child_on_cancel() {
        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("sleep child should spawn");
        let pid = Pid::from_raw(child.id() as i32);
        drop(child);

        let token = CancellationToken::new();
        let task = tokio::spawn(reap_session_child(pid, token.clone()));

        token.cancel();
        task.await.expect("reaper task should finish");

        assert!(matches!(
            waitpid(pid, Some(WaitPidFlag::WNOHANG)),
            Err(Errno::ECHILD)
        ));
    }
}
