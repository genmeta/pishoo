//! RemoteControlPlane: ControlPlane implementation for worker processes.
//!
//! Wraps a [`ControlPlaneClient`] (remoc RPC client) and implements
//! [`gateway::control_plane::ControlPlane`]. The returned
//! [`IpcListener`] / [`IpcConnector`] wrap the RPC clients received from
//! root, combined with the worker-side [`FdRegistry`] for MuxChannel FD
//! reception.
//!
//! For SSH session spawning, the control plane itself implements
//! [`SpawnSession`]: calls the remoc RPC, then receives the session
//! child's MuxChannel FD via the root-side MuxChannel's FdRegistry.

use gateway::control_plane::{ConnectorRequest, ListenRequest};
use h3x::ipc::{
    quic::{IpcConnector, IpcListener},
    transport::FdRegistry,
};
#[cfg(feature = "sshd")]
use h3x::varint::VarInt;
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
    /// FdRegistry from the worker-side MuxChannel for receiving FDs from root.
    fd_registry: FdRegistry,
}

impl RemoteControlPlane {
    pub fn new(client: ControlPlaneClient, fd_registry: FdRegistry) -> Self {
        Self {
            client,
            fd_registry,
        }
    }
}

/// Error from a remote session spawn request.
#[cfg(feature = "sshd")]
#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum RemoteSpawnSessionError {
    #[snafu(display("RPC to root failed"))]
    Rpc {
        source: crate::ipc::SpawnSessionError,
    },
    #[snafu(display("invalid FD batch id {id} from root"))]
    InvalidFdId {
        id: u64,
        source: h3x::varint::err::Overflow,
    },
    #[snafu(display("failed to receive session FD from root"))]
    ReceiveFd {
        source: h3x::ipc::transport::WaitFdsError,
    },
    #[snafu(display("expected 1 FD from root, got {actual}"))]
    UnexpectedFdCount { actual: usize },
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

        // RPC to root: fork session process + queue session FD via MuxChannel.
        // Returns the FD batch id for FdRegistry::wait_fds().
        let client = self.client.clone();
        let fd_id_raw = client
            .spawn_session(username.to_owned())
            .await
            .context(RpcSnafu)?;

        let fd_id = VarInt::try_from(fd_id_raw).context(InvalidFdIdSnafu { id: fd_id_raw })?;

        // Root has queued the session FD — receive it from FdRegistry.
        let fds = self
            .fd_registry
            .wait_fds(fd_id)
            .await
            .context(ReceiveFdSnafu)?;

        snafu::ensure!(fds.len() == 1, UnexpectedFdCountSnafu { actual: fds.len() });

        let mux_fd = fds.into_iter().next().expect("checked len == 1");

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

impl RemoteControlPlane {
    /// Atomically replace `_old` with a listener matching `request`.
    ///
    /// The old listener is consumed and dropped without explicit shutdown:
    /// root destroys its side of the listener as part of the rebuild critical
    /// section, so calling `shutdown` on the old IpcListener would race
    /// against a server that has already gone away.
    pub async fn rebuild_listener(
        &self,
        _old: IpcListener<IpcCodec>,
        request: ListenRequest,
    ) -> Result<IpcListener<IpcCodec>, RemoteRebuildError> {
        let ipc_client = self.client.rebuild_listener(request).await?;
        Ok(IpcListener::new(ipc_client, self.fd_registry.clone()))
    }
}

impl gateway::control_plane::ProvideListener for RemoteControlPlane {
    type Listener = IpcListener<IpcCodec>;
    type ListenError = RemoteListenError;

    async fn listener(&self, request: ListenRequest) -> Result<Self::Listener, Self::ListenError> {
        let ipc_client = self.client.listener(request).await?;
        Ok(IpcListener::new(ipc_client, self.fd_registry.clone()))
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
        Ok(IpcConnector::new(ipc_client, self.fd_registry.clone()))
    }
}
