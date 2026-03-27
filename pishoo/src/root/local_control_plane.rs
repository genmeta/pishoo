//! LocalControlPlane: in-process ControlPlane for root-local services.
//!
//! This allows root-local servers to use the same `run_service()` code
//! as workers, but without any IPC — operations go directly to RootState.

use std::sync::Arc;

use gateway::control_plane::{ConnectRequest, ListenRequest, StringError};
use snafu::{ResultExt, Snafu};
use tokio_util::sync::CancellationToken;

use crate::{
    per_server_listen::PerServerListenAdapter,
    root::state::{ServerEntry, ServiceOwner},
};

/// In-process [`gateway::control_plane::ControlPlane`] for root-local services.
///
/// Uses the same [`RootState`](super::state::RootState) as remote workers
/// but operates directly without RPC overhead. Returns a
/// [`PerServerListenAdapter`] that implements [`h3x::quic::Listen`].
pub struct LocalControlPlane {
    state: Arc<super::state::RootState>,
}

impl LocalControlPlane {
    pub fn new(state: Arc<super::state::RootState>) -> Self {
        Self { state }
    }
}

/// Error from a local listen request.
#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum LocalListenError {
    #[snafu(display("server `{server_name}` conflicts with an existing listener"))]
    Conflict { server_name: String },
    #[snafu(display("failed to add server `{server_name}` to listeners"))]
    AddServer {
        server_name: String,
        source: gm_quic::prelude::ServerError,
    },
}

/// Error from a local connect request.
#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum LocalConnectError {
    #[snafu(display("failed to build quic client"))]
    BuildClient { source: StringError },
}

impl gateway::control_plane::ControlPlane for LocalControlPlane {
    type Listener = PerServerListenAdapter;
    type Connector = Arc<gm_quic::prelude::QuicClient>;
    type ListenError = LocalListenError;
    type ConnectError = LocalConnectError;

    async fn listen(&self, request: ListenRequest) -> Result<Self::Listener, Self::ListenError> {
        let server_name = request.identity.name.as_full().to_owned();

        // Add to QuicListeners (involves network I/O).
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
            .context(local_listen_error::AddServerSnafu {
                server_name: &server_name,
            })?;

        // Register in state.
        let (tx, rx) = tokio::sync::mpsc::channel(128);
        let shutdown_token = CancellationToken::new();

        if self
            .state
            .register_server(
                server_name.clone(),
                ServerEntry {
                    owner: ServiceOwner::Local,
                    conn_tx: tx,
                    shutdown_token: shutdown_token.clone(),
                },
            )
            .is_err()
        {
            self.state.listeners.remove_server(&server_name);
            return Err(LocalListenError::Conflict { server_name });
        }

        Ok(PerServerListenAdapter::new(rx, shutdown_token))
    }

    async fn connect(
        &self,
        request: ConnectRequest,
    ) -> Result<Self::Connector, Self::ConnectError> {
        let root_store = crate::tls::root_cert_store();
        let builder = gm_quic::prelude::QuicClient::builder().with_root_certificates(root_store);
        let quic_client = match request.identity {
            Some(identity) => builder
                .with_cert(identity.certs, identity.key)
                .with_alpns(vec!["h3"])
                .build(),
            None => builder.without_cert().with_alpns(vec!["h3"]).build(),
        };
        Ok(Arc::new(quic_client))
    }
}
