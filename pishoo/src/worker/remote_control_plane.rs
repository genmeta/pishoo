//! RemoteControlPlane: ControlPlane implementation for worker processes.
//!
//! Wraps a [`ControlPlaneClient`] (remoc RPC client) and implements
//! [`gateway::control_plane::ControlPlane`]. The returned
//! [`RemoteListener`] / [`RemoteConnector`] come directly from the RPC
//! call — no additional wrapping needed.

use gateway::control_plane::{ConnectorRequest, ListenRequest};
use h3x::remoc::quic::{RemoteConnector, RemoteListener};
use snafu::Snafu;

// Import the RTC trait so that methods are visible on ControlPlaneClient.
use crate::ipc::ControlPlane as _;
use crate::ipc::ControlPlaneClient;

/// ControlPlane implementation backed by remoc RPC to the root process.
pub struct RemoteControlPlane {
    client: ControlPlaneClient,
}

impl RemoteControlPlane {
    pub fn new(client: ControlPlaneClient) -> Self {
        Self { client }
    }
}

/// Error from a remote listen request.
#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum RemoteListenError {
    #[snafu(transparent)]
    Protocol { source: crate::ipc::ListenError },
}

/// Error from a remote connect request.
#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum RemoteConnectError {
    #[snafu(transparent)]
    Protocol { source: crate::ipc::ConnectError },
}

impl gateway::control_plane::ControlPlane for RemoteControlPlane {
    type Listener = RemoteListener;
    type Connector = RemoteConnector;
    type ListenError = RemoteListenError;
    type ConnectError = RemoteConnectError;

    async fn listener(&self, request: ListenRequest) -> Result<Self::Listener, Self::ListenError> {
        Ok(self.client.listener(request).await?)
    }

    async fn connector(
        &self,
        request: ConnectorRequest,
    ) -> Result<Self::Connector, Self::ConnectError> {
        Ok(self.client.connector(request).await?)
    }
}
