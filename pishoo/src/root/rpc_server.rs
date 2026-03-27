//! ControlPlane implementation for the root process (remoc RPC server side).
//!
//! Each worker process gets its own [`ControlPlaneImpl`] instance, bound to
//! the worker's PID. When a worker calls `listen()` or `connect()`, this
//! module creates the actual QUIC resources and returns h3x remoc handles.

use std::sync::Arc;

use gateway::control_plane::{ConnectRequest, ListenRequest};
use h3x::remoc::quic::{ConnectServer, ListenServer, RemoteConnector, RemoteListener};
use nix::unistd::Pid;
use remoc::prelude::Server;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::{
    ipc::{ConnectError, ListenError},
    per_server_listen::PerServerListenAdapter,
    root::state::{ServerEntry, ServiceOwner},
};

/// Per-worker [`ControlPlane`](crate::ipc::ControlPlane) implementation.
///
/// Created for each worker process with a fixed `caller_pid`. Delegates
/// resource creation to the shared QUIC infrastructure and tracks ownership
/// in the root state.
pub struct ControlPlaneImpl {
    caller_pid: Pid,
    state: Arc<super::state::RootState>,
}

impl ControlPlaneImpl {
    pub fn new(caller_pid: Pid, state: Arc<super::state::RootState>) -> Self {
        Self { caller_pid, state }
    }
}

impl crate::ipc::ControlPlane for ControlPlaneImpl {
    async fn listen(&self, request: ListenRequest) -> Result<RemoteListener, ListenError> {
        let server_name = request.identity.name.as_full().to_owned();

        // Phase 1: validate (fast, no I/O).
        if self.state.has_server(&server_name) {
            tracing::warn!(
                caller_pid = %self.caller_pid,
                %server_name,
                "listen request conflicts with existing listener"
            );
            return Err(ListenError::Conflict);
        }

        // Phase 2: bind the server to QuicListeners (slow, involves network I/O).
        self.state
            .listeners
            .add_server(
                &server_name,
                request.identity.certs.as_slice(),
                &request.identity.key,
                request.bind,
                None::<Vec<u8>>,
            )
            .await
            .map_err(|error| {
                tracing::warn!(
                    %server_name,
                    error = %snafu::Report::from_error(&error),
                    "failed to add server to listeners"
                );
                ListenError::Internal {
                    message: format!("failed to add server `{server_name}`"),
                }
            })?;

        // Phase 3: commit registration and create the adapter.
        let (tx, rx) = tokio::sync::mpsc::channel(128);
        let shutdown_token = CancellationToken::new();

        // Re-check: another request may have raced during add_server.
        if let Err(error) = self.state.register_server(
            server_name.clone(),
            ServerEntry {
                owner: ServiceOwner::Worker(self.caller_pid),
                conn_tx: tx,
                shutdown_token: shutdown_token.clone(),
            },
        ) {
            self.state.listeners.remove_server(&server_name);
            tracing::warn!(
                caller_pid = %self.caller_pid,
                error = %snafu::Report::from_error(&error),
                "failed to register server in state"
            );
            return Err(ListenError::Conflict);
        }

        // Phase 4: serve the adapter via h3x remoc Listen RTC.
        let adapter = PerServerListenAdapter::new(rx, shutdown_token);
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

    async fn connect(&self, request: ConnectRequest) -> Result<RemoteConnector, ConnectError> {
        // Verify caller is a registered worker.
        if !self.state.has_worker(self.caller_pid) {
            return Err(ConnectError::Internal {
                message: format!("unknown caller pid {}", self.caller_pid),
            });
        }

        // Build a QuicClient with the requested client identity (if any).
        let root_store = crate::tls::root_cert_store();
        let builder = gm_quic::prelude::QuicClient::builder().with_root_certificates(root_store);
        let quic_client = match request.identity {
            Some(identity) => builder
                .with_cert(identity.certs, identity.key)
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

        self.state.add_connector_token(self.caller_pid, cancel);

        tracing::info!(
            caller_pid = %self.caller_pid,
            "Connect request fulfilled"
        );
        Ok(RemoteConnector::new(client))
    }
}
