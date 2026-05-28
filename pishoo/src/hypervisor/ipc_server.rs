//! ControlPlane implementation for the root process (remoc RPC server side).
//!
//! Each worker process gets its own [`WorkerControlPlane`] instance, bound to
//! the worker's PID. When a worker calls `listen()` or `connect()`, this
//! module creates the actual QUIC resources and returns IPC capability handles.

use std::sync::Arc;
#[cfg(feature = "sshd")]
use std::time::Duration;

use dhttp::name::DhttpName;
use gateway::control_plane::{ConnectorRequest, ListenRequest};
use h3x::ipc::{
    quic::{
        ConnectAdapter, IpcConnectClient, IpcConnectServerShared, IpcListenServerSharedMut,
        ListenAdapter,
    },
    transport::FdSender,
};
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

use super::{endpoint_factory, state::RootState};
use crate::{
    hypervisor::state::AcquireListenerError,
    ipc::{ConnectError, ListenError},
};

/// Per-worker [`ControlPlane`](crate::ipc::ControlPlane) implementation.
///
/// Created for each worker process with a fixed `caller_pid`. Delegates
/// resource creation to the shared QUIC infrastructure and tracks ownership
/// in the root state.
pub struct WorkerControlPlane {
    caller_pid: Pid,
    state: Arc<RootState>,
    /// FdSender from the root-side MuxChannel, used to pass FDs to worker.
    fd_sender: FdSender,
}

impl WorkerControlPlane {
    pub fn new(caller_pid: Pid, state: Arc<RootState>, fd_sender: FdSender) -> Self {
        Self {
            caller_pid,
            state,
            fd_sender,
        }
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
    ) -> Result<h3x::ipc::quic::IpcListenClient, ListenError> {
        let server_name = request.identity.name().as_full().to_owned();
        let owner = self
            .state
            .owner_for_pid(self.caller_pid)
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
                    | AcquireListenerError::OwnerUnavailable => ListenError::Internal {
                        message: format!("failed to prepare endpoint for `{server_name}`"),
                    },
                }
            })?;

        // Wrap adapter in ListenAdapter for IPC capability forwarding.
        let listen_adapter = ListenAdapter::<_, IpcCodec>::new(adapter, self.fd_sender.clone());
        let (server, client) =
            IpcListenServerSharedMut::new(Arc::new(tokio::sync::RwLock::new(listen_adapter)), 64);
        let spawned = self
            .state
            .spawn_worker_task(self.caller_pid, |token| {
                async move {
                    tokio::select! {
                        () = token.cancelled() => {}
                        _ = server.serve(true) => {}
                    }
                }
                .in_current_span()
            })
            .await;
        if !spawned {
            let server_name = DhttpName::try_from(server_name)
                .expect("listen request identity must be a dhttp name");
            let _ = self.state.release_listener(owner, &server_name).await;
            return Err(ListenError::Internal {
                message: format!(
                    "worker {} exited before IPC listener was ready",
                    self.caller_pid
                ),
            });
        }

        tracing::debug!(
            caller_pid = %self.caller_pid,
            %server_name,
            "listen request fulfilled (IPC)"
        );
        Ok(client)
    }

    async fn connector(&self, request: ConnectorRequest) -> Result<IpcConnectClient, ConnectError> {
        // Verify caller is a registered worker.
        if !self.state.has_worker(self.caller_pid).await {
            return Err(ConnectError::Internal {
                message: format!("unknown caller pid {}", self.caller_pid),
            });
        }

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
        let connect_adapter = ConnectAdapter::<_, IpcCodec>::new(endpoint, self.fd_sender.clone());
        let (server, client) = IpcConnectServerShared::new(Arc::new(connect_adapter), 64);
        let spawned = self
            .state
            .spawn_worker_task(self.caller_pid, |token| {
                async move {
                    tokio::select! {
                        () = token.cancelled() => {}
                        _ = server.serve(true) => {}
                    }
                }
                .in_current_span()
            })
            .await;
        if !spawned {
            return Err(ConnectError::Internal {
                message: format!(
                    "worker {} exited before IPC connector was ready",
                    self.caller_pid
                ),
            });
        }

        tracing::debug!(
            caller_pid = %self.caller_pid,
            "connect request fulfilled (IPC)"
        );
        Ok(client)
    }

    async fn spawn_session(&self, username: String) -> Result<u64, crate::ipc::SpawnSessionError> {
        #[cfg(feature = "sshd")]
        {
            tracing::info!(
                caller_pid = %self.caller_pid,
                %username,
                "spawn session request received"
            );

            // Fork the session process as root (no privilege drop).
            let transport =
                crate::hypervisor::launcher::launch_session(&username).map_err(|e| {
                    crate::ipc::SpawnSessionError::SpawnFailed {
                        reason: snafu::Report::from_error(e).to_string(),
                    }
                })?;

            // Send the session child's MuxChannel FD to the worker via the
            // root-side MuxChannel's FD sender. The worker will pick it up
            // from FdRegistry using the returned VarInt id.
            let child_pid = transport.child_pid;
            let fd_id = match self.fd_sender.queue_fds(vec![transport.mux_fd].into()) {
                Ok(fd_id) => fd_id,
                Err(error) => {
                    terminate_session_child(child_pid).await;
                    return Err(crate::ipc::SpawnSessionError::SpawnFailed {
                        reason: snafu::Report::from_error(error).to_string(),
                    });
                }
            };

            // Reap the session child process to avoid zombies. The reaper is
            // scoped to the worker so worker cleanup can cancel it and then
            // wait for the session child to be terminated and reaped.
            let state = self.state.clone();
            let caller_pid = self.caller_pid;
            let spawned_reaper = state
                .spawn_worker_task(caller_pid, move |token| async move {
                    reap_session_child(child_pid, token).await;
                })
                .await;
            if !spawned_reaper {
                terminate_session_child(child_pid).await;
                return Err(crate::ipc::SpawnSessionError::SpawnFailed {
                    reason: format!(
                        "worker {caller_pid} exited before session reaper was registered"
                    ),
                });
            }

            tracing::info!(
                caller_pid = %self.caller_pid,
                %username,
                fd_id = %fd_id,
                "session spawned, FD queued for worker"
            );
            Ok(u64::from(fd_id))
        }
        #[cfg(not(feature = "sshd"))]
        {
            let _ = username;
            Err(crate::ipc::SpawnSessionError::NotSupported)
        }
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
