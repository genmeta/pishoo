//! ControlPlane implementation for the root process (remoc RPC server side).
//!
//! Each worker process gets its own [`WorkerControlPlaneRpc`] instance, bound to
//! the worker's PID. When a worker calls `listen()` or `connect()`, this
//! module creates the actual QUIC resources and returns h3x remoc handles.

#[cfg(feature = "sshd")]
use std::os::fd::OwnedFd;
use std::sync::Arc;

use gateway::control_plane::{ConnectorRequest, ListenRequest};
use h3x::remoc::quic::{ConnectServer, ListenServer, RemoteConnector, RemoteListener};
use nix::unistd::Pid;
use remoc::prelude::Server;
use tracing::Instrument;

use crate::{
    ipc::{ConnectError, ListenError},
    root::state::{RegisterError, ServiceOwner},
};

/// Per-worker [`ControlPlane`](crate::ipc::ControlPlane) implementation.
///
/// Created for each worker process with a fixed `caller_pid`. Delegates
/// resource creation to the shared QUIC infrastructure and tracks ownership
/// in the root state.
pub struct WorkerControlPlane {
    caller_pid: Pid,
    state: Arc<super::state::RootState>,
    /// Root-side end of the seqpacket pair for sending FDs to the worker.
    #[cfg(feature = "sshd")]
    seqpacket: OwnedFd,
}

impl WorkerControlPlane {
    pub fn new(
        caller_pid: Pid,
        state: Arc<super::state::RootState>,
        #[cfg(feature = "sshd")] seqpacket: OwnedFd,
    ) -> Self {
        Self {
            caller_pid,
            state,
            #[cfg(feature = "sshd")]
            seqpacket,
        }
    }
}

impl crate::ipc::ControlPlane for WorkerControlPlane {
    async fn listener(&self, request: ListenRequest) -> Result<RemoteListener, ListenError> {
        let server_name = request.identity.name().as_full().to_owned();
        let owner = ServiceOwner::Worker(self.caller_pid);

        let adapter = self
            .state
            .register_listener(owner, request)
            .await
            .map_err(|error| {
                tracing::warn!(
                    caller_pid = %self.caller_pid,
                    %server_name,
                    error = %snafu::Report::from_error(&error),
                    "Listen request failed"
                );
                match error {
                    RegisterError::DuplicateListen | RegisterError::ConflictedName => {
                        ListenError::Conflict
                    }
                    RegisterError::AddServerFailed { .. } => ListenError::Internal {
                        message: format!("failed to add server `{server_name}`"),
                    },
                }
            })?;

        // Serve the adapter via h3x remoc Listen RTC.
        let (server, client) = ListenServer::new(adapter, 1);
        self.state
            .spawn_worker_task(
                self.caller_pid,
                async move {
                    let _ = server.serve().await;
                }
                .in_current_span(),
            )
            .await;

        tracing::info!(
            caller_pid = %self.caller_pid,
            %server_name,
            "Listen request fulfilled"
        );
        Ok(RemoteListener::new(client))
    }

    async fn connector(&self, request: ConnectorRequest) -> Result<RemoteConnector, ConnectError> {
        // Verify caller is a registered worker.
        if !self.state.has_worker(self.caller_pid).await {
            return Err(ConnectError::Internal {
                message: format!("unknown caller pid {}", self.caller_pid),
            });
        }

        // Build a QuicClient with the requested client identity (if any).
        let root_store = crate::tls::root_cert_store();
        let builder = dquic::prelude::QuicClient::builder().with_root_certificates(root_store);
        let quic_client = match request.identity {
            Some(identity) => builder
                .with_cert(identity.certs().to_vec(), identity.key().clone_key())
                .with_alpns(vec!["h3"])
                .build(),
            None => builder.without_cert().with_alpns(vec!["h3"]).build(),
        };
        let quic_client = Arc::new(quic_client);

        // Create a ConnectServer wrapping the per-identity QuicClient.
        let (server, client) = ConnectServer::new(quic_client, 1);

        self.state
            .spawn_worker_task(
                self.caller_pid,
                async move {
                    let _ = server.serve().await;
                }
                .in_current_span(),
            )
            .await;

        tracing::info!(
            caller_pid = %self.caller_pid,
            "Connect request fulfilled"
        );
        Ok(RemoteConnector::new(client))
    }

    async fn spawn_session(&self, username: String) -> Result<(), crate::ipc::SpawnSessionError> {
        #[cfg(feature = "sshd")]
        {
            use std::os::fd::AsRawFd;

            tracing::info!(
                caller_pid = %self.caller_pid,
                %username,
                "spawn session request received"
            );

            // Fork the session process as root (no privilege drop).
            let transport = crate::root::launcher::launch_session(&username).map_err(|e| {
                crate::ipc::SpawnSessionError::SpawnFailed {
                    reason: snafu::Report::from_error(e).to_string(),
                }
            })?;

            // Send the child's pipe FDs to the worker via SCM_RIGHTS on the
            // seqpacket socket. This must complete BEFORE we return Ok(()) —
            // the worker reads FDs immediately after the RPC returns.
            let sock_fd = self.seqpacket.as_raw_fd();
            let stdin_fd = transport.stdin.as_raw_fd();
            let stdout_fd = transport.stdout.as_raw_fd();
            tokio::task::spawn_blocking(move || {
                let fds = [stdin_fd, stdout_fd];
                crate::root::launcher::send_fds(sock_fd, &fds)
            })
            .await
            .map_err(|_| crate::ipc::SpawnSessionError::SpawnFailed {
                reason: "blocking task cancelled".to_owned(),
            })?
            .map_err(|e| crate::ipc::SpawnSessionError::SpawnFailed {
                reason: format!("failed to send FDs: {e}"),
            })?;

            // transport.stdin/stdout OwnedFds drop here, closing root's copies.
            // The worker now owns the only copies (via SCM_RIGHTS).
            let child_pid = transport.child_pid;
            drop(transport);

            // Reap the session child process to avoid zombies.
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
                "session spawned, FDs sent to worker"
            );
            Ok(())
        }
        #[cfg(not(feature = "sshd"))]
        {
            let _ = username;
            Err(crate::ipc::SpawnSessionError::NotSupported)
        }
    }
}
