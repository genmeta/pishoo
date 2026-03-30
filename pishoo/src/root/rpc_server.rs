//! ControlPlane implementation for the root process (remoc RPC server side).
//!
//! Each worker process gets its own [`WorkerControlPlaneRpc`] instance, bound to
//! the worker's PID. When a worker calls `listen()` or `connect()`, this
//! module creates the actual QUIC resources and returns h3x remoc handles.

use std::sync::Arc;

use gateway::control_plane::{ConnectorRequest, ListenRequest};
use h3x::remoc::quic::{ConnectServer, ListenServer, RemoteConnector, RemoteListener};
use nix::unistd::Pid;
use remoc::prelude::Server;
use tokio_util::sync::CancellationToken;
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
}

impl WorkerControlPlane {
    pub fn new(caller_pid: Pid, state: Arc<super::state::RootState>) -> Self {
        Self { caller_pid, state }
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
        tokio::spawn(
            async move {
                let _ = server.serve().await;
            }
            .in_current_span(),
        );

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
        let builder = gm_quic::prelude::QuicClient::builder().with_root_certificates(root_store);
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

        // Track with a cancellation token for cleanup on worker exit.
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        tokio::spawn(
            async move {
                tokio::select! {
                    _ = async { let _ = server.serve().await; } => {}
                    () = cancel_clone.cancelled() => {}
                }
            }
            .in_current_span(),
        );

        self.state
            .add_connector_token(self.caller_pid, cancel)
            .await;

        tracing::info!(
            caller_pid = %self.caller_pid,
            "Connect request fulfilled"
        );
        Ok(RemoteConnector::new(client))
    }
}
