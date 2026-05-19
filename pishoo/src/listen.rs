//! Root-owned endpoint wrapper for local and worker services.
//!
//! The root process owns the shared DHTTP network. Each registered server gets
//! a [`dhttp::endpoint::Endpoint`] built from its identity and bind patterns;
//! workers receive this wrapper through IPC as an [`h3x::quic::Listen`]
//! capability. Shutdown routes back through [`RootState`] so registry ownership
//! and endpoint lifetime stay synchronized.

use std::sync::{Arc, Weak};

use dhttp::{endpoint::Endpoint, name::DhttpName};
use snafu::Snafu;
use tokio_util::sync::CancellationToken;

use crate::hypervisor::state::{RootState, ServiceOwner};

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum RegisteredEndpointError {
    #[snafu(display("registered endpoint shut down"))]
    Shutdown,
    #[snafu(transparent)]
    Accept { source: h3x::dquic::AcceptError },
}

/// Root-held endpoint belonging to one registered service owner.
pub struct RegisteredEndpoint {
    endpoint: Endpoint,
    shutdown_token: CancellationToken,
    root_state: Weak<RootState>,
    server_name: DhttpName<'static>,
    owner: ServiceOwner,
}

impl RegisteredEndpoint {
    pub fn new_registered(
        endpoint: Endpoint,
        shutdown_token: CancellationToken,
        root_state: &Arc<RootState>,
        server_name: DhttpName<'static>,
        owner: ServiceOwner,
    ) -> Self {
        Self {
            endpoint,
            shutdown_token,
            root_state: Arc::downgrade(root_state),
            server_name,
            owner,
        }
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }
}

impl h3x::quic::Listen for RegisteredEndpoint {
    type Connection = h3x::dquic::prelude::Connection;
    type Error = RegisteredEndpointError;

    async fn accept(&mut self) -> Result<Arc<Self::Connection>, Self::Error> {
        tokio::select! {
            result = self.endpoint.accept() => Ok(result?),
            () = self.shutdown_token.cancelled() => Err(RegisteredEndpointError::Shutdown),
        }
    }

    async fn shutdown(&self) -> Result<(), Self::Error> {
        self.shutdown_token.cancel();
        if let Some(root_state) = self.root_state.upgrade() {
            root_state
                .release_server(&self.server_name, self.owner)
                .await;
        }
        self.endpoint.shutdown().await?;
        Ok(())
    }
}
