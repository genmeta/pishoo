//! ControlPlane implementation for the root process (remoc RPC server side).
//!
//! Each worker process gets its own [`WorkerControlPlane`] instance, bound to
//! the worker's PID. When a worker calls `listen()` or `connect()`, this
//! module creates the actual QUIC resources and returns IPC capability handles.

use std::sync::Arc;

use dhttp::{ddns::DnsScheme, endpoint::Endpoint};
use gateway::control_plane::{ConnectorRequest, ListenRequest};
use h3x::ipc::{
    quic::{
        ConnectAdapter, IpcConnectClient, IpcConnectServerShared, IpcListenServerSharedMut,
        ListenAdapter,
    },
    transport::FdSender,
};
use nix::unistd::Pid;
use remoc::prelude::{ServerShared, ServerSharedMut};
use tracing::Instrument;

use super::state::RootState;
use crate::{
    hypervisor::state::{RegisterError, ServiceOwner},
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
        let owner = ServiceOwner::Worker(self.caller_pid);

        let adapter =
            self.state
                .register_listener(owner, request)
                .await
                .map_err(|error| {
                    tracing::warn!(
                        caller_pid = %self.caller_pid,
                        %server_name,
                        error = %snafu::Report::from_error(&error),
                        "listen request failed"
                    );
                    match error {
                        RegisterError::DuplicateListen | RegisterError::ConflictedName => {
                            ListenError::Conflict
                        }
                        RegisterError::BuildResolver { .. }
                        | RegisterError::CreatePublisher { .. } => ListenError::Internal {
                            message: format!("failed to prepare endpoint for `{server_name}`"),
                        },
                    }
                })?;

        // Wrap adapter in ListenAdapter for IPC capability forwarding.
        let listen_adapter = ListenAdapter::<_, IpcCodec>::new(adapter, self.fd_sender.clone());
        let (server, client) =
            IpcListenServerSharedMut::new(Arc::new(tokio::sync::RwLock::new(listen_adapter)), 64);
        self.state
            .spawn_worker_task(
                self.caller_pid,
                async move {
                    let _ = server.serve(true).await;
                }
                .in_current_span(),
            )
            .await;

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
        let endpoint = Arc::new(
            Endpoint::builder()
                .network(self.state.network.clone())
                .maybe_identity(request.identity.map(Arc::new))
                .dns(DnsScheme::H3)
                .dns(DnsScheme::Mdns)
                .dns(DnsScheme::System)
                .build()
                .await,
        );

        // Wrap the endpoint in ConnectAdapter for IPC capability forwarding.
        let connect_adapter = ConnectAdapter::<_, IpcCodec>::new(endpoint, self.fd_sender.clone());
        let (server, client) = IpcConnectServerShared::new(Arc::new(connect_adapter), 64);
        self.state
            .spawn_worker_task(
                self.caller_pid,
                async move {
                    let _ = server.serve(true).await;
                }
                .in_current_span(),
            )
            .await;

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
            let fd_id = self
                .fd_sender
                .queue_fds(vec![transport.mux_fd].into())
                .map_err(|e| crate::ipc::SpawnSessionError::SpawnFailed {
                    reason: snafu::Report::from_error(e).to_string(),
                })?;

            // Reap the session child process to avoid zombies.
            let child_pid = transport.child_pid;
            let state = self.state.clone();
            let caller_pid = self.caller_pid;
            state
                .spawn_worker_task(caller_pid, async move {
                    let _ = tokio::task::spawn_blocking(move || {
                        let _ = nix::sys::wait::waitpid(child_pid, None);
                    })
                    .await;
                })
                .await;

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
