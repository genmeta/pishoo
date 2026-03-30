//! LocalControlPlane: in-process ControlPlane for root-local services.
//!
//! This allows root-local servers to use the same `run_service()` code
//! as workers, but without any IPC — operations go directly to RootState.

use std::sync::Arc;

use gateway::control_plane::{ConnectorRequest, ListenRequest, StringError};
use snafu::Snafu;

use crate::{
    per_server_listen::PerServerListener,
    root::state::{RegisterError, ServiceOwner},
};

/// In-process [`gateway::control_plane::ControlPlane`] for root-local services.
///
/// Uses the same [`RootState`](super::state::RootState) as remote workers
/// but operates directly without RPC overhead. Returns a
/// [`PerServerListener`] that implements [`h3x::quic::Listen`].
pub struct LocalControlPlane {
    state: Arc<super::state::RootState>,
}

impl LocalControlPlane {
    pub fn new(state: Arc<super::state::RootState>) -> Self {
        Self { state }
    }
}

/// Error from a local connect request.
#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum LocalConnectError {
    #[snafu(display("failed to build quic client"))]
    BuildClient { source: StringError },
}

impl gateway::control_plane::ControlPlane for LocalControlPlane {
    type Listener = PerServerListener;
    type Connector = Arc<gm_quic::prelude::QuicClient>;
    type ListenError = RegisterError;
    type ConnectError = LocalConnectError;

    async fn listener(&self, request: ListenRequest) -> Result<Self::Listener, Self::ListenError> {
        self.state
            .register_listener(ServiceOwner::Local, request)
            .await
    }

    async fn connector(
        &self,
        request: ConnectorRequest,
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
