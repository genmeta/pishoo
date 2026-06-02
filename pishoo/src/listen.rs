//! Root-owned endpoint wrapper for local and worker services.
//!
//! The root process owns the shared DHTTP network. Each registered server gets
//! a [`dhttp::endpoint::Endpoint`] built from its identity and bind patterns;
//! workers receive this wrapper through IPC as an [`h3x::quic::Listen`]
//! capability. Shutdown is an explicit release operation; dropping this handle
//! only cancels local accept waits.

use std::sync::{
    Arc, Weak,
    atomic::{AtomicBool, Ordering},
};

use dhttp::{endpoint::Endpoint, name::DhttpName};
use h3x::quic::Listen as _;
use snafu::Snafu;
use tokio_util::sync::CancellationToken;

use crate::hypervisor::state::{ReleaseListenerError, RootState, owner::Owner};

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum RegisteredEndpointError {
    #[snafu(display("registered endpoint shut down"))]
    Shutdown,
    #[snafu(transparent)]
    Accept { source: h3x::dquic::AcceptError },
    #[snafu(display("failed to release registered listener"))]
    Release { source: ReleaseListenerError },
}

/// Root-held endpoint belonging to one registered service owner.
pub struct RegisteredEndpoint {
    endpoint: Endpoint,
    shutdown_token: CancellationToken,
    root_state: Weak<RootState>,
    server_name: DhttpName<'static>,
    owner: Owner,
    released: Arc<AtomicBool>,
}

impl RegisteredEndpoint {
    pub fn new_registered(
        endpoint: Endpoint,
        shutdown_token: CancellationToken,
        root_state: &Arc<RootState>,
        server_name: DhttpName<'static>,
        owner: Owner,
    ) -> Self {
        Self {
            endpoint,
            shutdown_token,
            root_state: Arc::downgrade(root_state),
            server_name,
            owner,
            released: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    pub(crate) async fn destroy_without_registry_release(self) {
        self.shutdown_token.cancel();
        if let Err(error) = self.endpoint.shutdown().await {
            tracing::warn!(
                error = %snafu::Report::from_error(&error),
                "failed to shut down unregistered endpoint"
            );
        }
    }
}

impl h3x::quic::Listen for RegisteredEndpoint {
    type Connection = h3x::dquic::prelude::Connection;
    type Error = RegisteredEndpointError;

    async fn accept(&mut self) -> Result<Arc<Self::Connection>, Self::Error> {
        tokio::select! {
            biased;
            () = self.shutdown_token.cancelled() => Err(RegisteredEndpointError::Shutdown),
            result = self.endpoint.accept() => Ok(result?),
        }
    }

    async fn shutdown(&self) -> Result<(), Self::Error> {
        self.shutdown_token.cancel();
        if !self.released.swap(true, Ordering::SeqCst)
            && let Some(root_state) = self.root_state.upgrade()
        {
            root_state
                .release_listener(self.owner, &self.server_name)
                .await
                .map_err(|source| RegisteredEndpointError::Release { source })?;
        }
        Ok(())
    }
}

impl Drop for RegisteredEndpoint {
    fn drop(&mut self) {
        self.shutdown_token.cancel();
    }
}
