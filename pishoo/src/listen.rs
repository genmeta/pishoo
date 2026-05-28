//! Root-owned endpoint wrapper for local and worker services.
//!
//! The root process owns the shared DHTTP network. Each registered server gets
//! a [`dhttp::endpoint::Endpoint`] built from its identity and bind patterns;
//! workers receive this wrapper through IPC as an [`h3x::quic::Listen`]
//! capability. Shutdown routes back through [`RootState`] so registry ownership
//! and endpoint lifetime stay synchronized.

use std::sync::{
    Arc, Weak,
    atomic::{AtomicBool, Ordering},
};

use dhttp::{endpoint::Endpoint, name::DhttpName};
use snafu::Snafu;
use tokio_util::sync::CancellationToken;

use crate::hypervisor::{
    state::{RootState, ServiceOwner},
    task_scope::TaskScope,
};

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
    release_scope: TaskScope,
    released: Arc<AtomicBool>,
}

impl RegisteredEndpoint {
    pub fn new_registered(
        endpoint: Endpoint,
        shutdown_token: CancellationToken,
        root_state: &Arc<RootState>,
        server_name: DhttpName<'static>,
        owner: ServiceOwner,
        release_scope: TaskScope,
    ) -> Self {
        Self {
            endpoint,
            shutdown_token,
            root_state: Arc::downgrade(root_state),
            server_name,
            owner,
            release_scope,
            released: Arc::new(AtomicBool::new(false)),
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
        if !self.released.swap(true, Ordering::SeqCst)
            && let Some(root_state) = self.root_state.upgrade()
        {
            root_state
                .release_server(&self.server_name, self.owner)
                .await;
        }
        self.endpoint.shutdown().await?;
        Ok(())
    }
}

impl Drop for RegisteredEndpoint {
    fn drop(&mut self) {
        self.shutdown_token.cancel();
        if self.released.swap(true, Ordering::SeqCst) {
            return;
        }

        let Some(root_state) = self.root_state.upgrade() else {
            return;
        };
        if self.release_scope.is_cancelled() {
            return;
        }
        let server_name = self.server_name.clone();
        let owner = self.owner;
        self.release_scope.spawn(move |_token| async move {
            root_state.release_server(&server_name, owner).await;
        });
    }
}
