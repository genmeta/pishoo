//! RemoteControlPlane: ControlPlane implementation for worker processes.
//!
//! Wraps a [`ControlPlaneClient`] (remoc RPC client) and implements
//! [`gateway::control_plane::ControlPlane`]. The returned
//! [`IpcListener`] / [`IpcConnector`] wrap the RPC clients received from
//! root, combined with the worker-side [`FdTransfer`] for MuxChannel FD
//! reception.
//!
//! For SSH session spawning, the control plane itself implements
//! [`SpawnSession`]: calls the remoc RPC, then receives the session
//! child's MuxChannel FD via a receiver-chosen FD transfer ID.

use gateway::control_plane::{ConnectorRequest, ListenRequest};
use h3x::ipc::{
    quic::{IpcConnector, IpcListener},
    transport::FdTransfer,
};
use snafu::Snafu;

// Import the RTC trait so that methods are visible on ControlPlaneClient.
use crate::ipc::ControlPlane as _;
use crate::ipc::ControlPlaneClient;

/// The remoc codec for per-connection MuxChannel links.
/// Must match the server side ([`WorkerControlPlane`]).
type IpcCodec = remoc::codec::Default;

/// ControlPlane implementation backed by remoc RPC to the root process.
pub struct RemoteControlPlane {
    client: ControlPlaneClient,
    /// FD transfer plane from the worker-side MuxChannel.
    fd_transfer: FdTransfer,
}

impl RemoteControlPlane {
    pub fn new(client: ControlPlaneClient, fd_transfer: FdTransfer) -> Self {
        Self {
            client,
            fd_transfer,
        }
    }
}

/// Error from a remote session spawn request.
#[cfg(feature = "sshd")]
#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum RemoteSpawnSessionError {
    #[snafu(display("rpc to root failed"))]
    Rpc {
        source: crate::ipc::SpawnSessionError,
    },
    #[snafu(display("failed to receive session fd from root"))]
    ReceiveFd {
        source: h3x::ipc::transport::WaitFdsError,
    },
    #[snafu(display("unexpected session fd batch size"))]
    UnexpectedFdCount {
        source: h3x::ipc::transport::TakeFdsError,
    },
    #[snafu(display("root responded with fd id {actual}, expected {expected}"))]
    FdIdMismatch { expected: u64, actual: u64 },
}

#[cfg(feature = "sshd")]
impl gateway::control_plane::SpawnSession for RemoteControlPlane {
    type Error = RemoteSpawnSessionError;

    async fn spawn_session(
        &self,
        username: &str,
    ) -> Result<gateway::control_plane::SessionTransport, Self::Error> {
        use remote_spawn_session_error::*;
        use snafu::ResultExt;

        let receiver = self.fd_transfer.receive();
        let fd_id = receiver.id();
        let expected = u64::from(fd_id);

        // RPC to root: fork session process + deliver the session FD using
        // this receiver-chosen FD transfer ID.
        let client = self.client.clone();
        let actual = client
            .spawn_session(username.to_owned(), expected)
            .await
            .context(RpcSnafu)?;
        snafu::ensure!(actual == expected, FdIdMismatchSnafu { expected, actual });

        let received = receiver.await.context(ReceiveFdSnafu)?;
        let mux_fd = received.into_one().context(UnexpectedFdCountSnafu)?;

        Ok(gateway::control_plane::SessionTransport { mux_fd })
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

/// Error from a remote rebuild request.
#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum RemoteRebuildError {
    #[snafu(transparent)]
    Protocol {
        source: crate::ipc::RebuildListenError,
    },
}

impl gateway::control_plane::ProvideListener for RemoteControlPlane {
    type Listener = IpcListener<IpcCodec>;
    type ListenError = RemoteListenError;
    type RebuildError = RemoteRebuildError;

    async fn listener(&self, request: ListenRequest) -> Result<Self::Listener, Self::ListenError> {
        let ipc_client = self.client.listener(request).await?;
        Ok(IpcListener::new(ipc_client, self.fd_transfer.clone()))
    }

    async fn rebuild_listener(
        &self,
        _old: Self::Listener,
        request: ListenRequest,
    ) -> Result<Self::Listener, Self::RebuildError> {
        // _old is consumed without explicit shutdown: root destroys its side
        // of the listener as part of the rebuild critical section, so calling
        // shutdown on the old IpcListener would race against a server that
        // has already gone away.
        let ipc_client = self.client.rebuild_listener(request).await?;
        Ok(IpcListener::new(ipc_client, self.fd_transfer.clone()))
    }
}

impl gateway::control_plane::ProvideConnector for RemoteControlPlane {
    type Connector = IpcConnector<IpcCodec>;
    type ConnectError = RemoteConnectError;

    async fn connector(
        &self,
        request: ConnectorRequest,
    ) -> Result<Self::Connector, Self::ConnectError> {
        let ipc_client = self.client.connector(request).await?;
        Ok(IpcConnector::new(ipc_client, self.fd_transfer.clone()))
    }
}
