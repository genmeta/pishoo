//! Root-owned endpoint wrapper for local and worker services.
//!
//! The root process owns the shared DHTTP network. Each registered server gets
//! a [`dhttp::endpoint::Endpoint`] built from its identity and bind patterns;
//! workers receive this wrapper through IPC as an [`dhttp::h3x::quic::Listen`]
//! capability. Shutdown and drop both release through the root-owned async
//! resource transition path.

use std::sync::{Arc, Weak};

use dhttp::{endpoint::Endpoint, name::DhttpName};
use snafu::{ResultExt, Snafu};
use tokio_util::sync::CancellationToken;

use crate::hypervisor::{
    resource::AsyncReleaseGuard,
    state::{ReleaseListenerError, RootState, owner::Owner},
};

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum RegisteredEndpointError {
    #[snafu(display("registered endpoint shut down"))]
    Shutdown,
    #[snafu(transparent)]
    Accept {
        source: dhttp::h3x::dquic::AcceptError,
    },
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
    release_guard: AsyncReleaseGuard,
}

impl RegisteredEndpoint {
    pub(crate) fn new_registered(
        endpoint: Endpoint,
        shutdown_token: CancellationToken,
        root_state: &Arc<RootState>,
        server_name: DhttpName<'static>,
        owner: Owner,
        release_guard: AsyncReleaseGuard,
    ) -> Self {
        Self {
            endpoint,
            shutdown_token,
            root_state: Arc::downgrade(root_state),
            server_name,
            owner,
            release_guard,
        }
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }
}

impl dhttp::h3x::quic::Listen for RegisteredEndpoint {
    type Connection = dhttp::h3x::dquic::prelude::Connection;
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
        if self.release_guard.take()
            && let Some(root_state) = self.root_state.upgrade()
        {
            root_state
                .release_listener_for_handle(
                    self.owner,
                    &self.server_name,
                    self.release_guard.clone(),
                )
                .await
                .context(registered_endpoint_error::ReleaseSnafu)?;
        }
        Ok(())
    }
}

impl Drop for RegisteredEndpoint {
    fn drop(&mut self) {
        self.shutdown_token.cancel();
        if self.release_guard.take()
            && let Some(root_state) = self.root_state.upgrade()
        {
            root_state.release_listener_for_dropped_handle(
                self.owner,
                self.server_name.clone(),
                self.release_guard.clone(),
            );
        }
    }
}
