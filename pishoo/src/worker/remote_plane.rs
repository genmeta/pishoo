//! RemoteControlPlane: ControlPlane implementation for worker processes.
//!
//! Wraps a [`ControlPlaneClient`] (remoc RPC client) and implements
//! [`gateway::control_plane::ControlPlane`]. The returned
//! [`RemoteListener`] / [`RemoteConnector`] come directly from the RPC
//! call — no additional wrapping needed.
//!
//! For SSH session spawning, the control plane itself implements
//! [`SpawnSession`] and serializes spawn requests via a [`Mutex`]:
//! calls the remoc RPC, then reads the session pipe FDs from the
//! seqpacket via `SCM_RIGHTS`.

#[cfg(feature = "sshd")]
use std::os::fd::OwnedFd;
#[cfg(feature = "sshd")]
use std::sync::Arc;

use gateway::control_plane::{ConnectorRequest, ListenRequest};
use h3x::remoc::quic::{RemoteConnector, RemoteListener};
use snafu::Snafu;

// Import the RTC trait so that methods are visible on ControlPlaneClient.
use crate::ipc::ControlPlane as _;
use crate::ipc::ControlPlaneClient;

/// ControlPlane implementation backed by remoc RPC to the root process.
pub struct RemoteControlPlane {
    client: ControlPlaneClient,
    /// Worker-side end of the seqpacket pair for receiving FDs from root.
    #[cfg(feature = "sshd")]
    seqpacket: Arc<OwnedFd>,
    /// Serializes session spawn requests to ensure FD ordering on the seqpacket.
    #[cfg(feature = "sshd")]
    spawn_lock: tokio::sync::Mutex<()>,
}

impl RemoteControlPlane {
    pub fn new(client: ControlPlaneClient, #[cfg(feature = "sshd")] seqpacket: OwnedFd) -> Self {
        Self {
            client,
            #[cfg(feature = "sshd")]
            seqpacket: Arc::new(seqpacket),
            #[cfg(feature = "sshd")]
            spawn_lock: tokio::sync::Mutex::new(()),
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
    #[snafu(display("failed to receive FDs from root"))]
    ReceiveFds { source: nix::errno::Errno },
    #[snafu(display("expected 2 FDs from root, got {actual}"))]
    UnexpectedFdCount { actual: usize },
    #[snafu(display("recv_fds task panicked"))]
    JoinRecvFds { source: tokio::task::JoinError },
}

#[cfg(feature = "sshd")]
impl gateway::control_plane::SpawnSession for RemoteControlPlane {
    type Error = RemoteSpawnSessionError;

    async fn spawn_session(
        &self,
        username: &str,
    ) -> Result<gateway::control_plane::SessionTransport, Self::Error> {
        use std::os::fd::AsRawFd;

        use remote_spawn_session_error::*;
        use snafu::ResultExt;

        // Serialize: only one spawn at a time to ensure FD ordering.
        let _guard = self.spawn_lock.lock().await;

        // RPC to root: fork session process + send FDs via SCM_RIGHTS.
        // Clone the client to avoid lifetime issues with the async borrow.
        let client = self.client.clone();
        client
            .spawn_session(username.to_owned())
            .await
            .context(RpcSnafu)?;

        // Root has sent FDs before returning Ok(()) — read them now.
        let sock_fd = self.seqpacket.as_raw_fd();
        let fds =
            tokio::task::spawn_blocking(move || crate::hypervisor::launcher::recv_fds(sock_fd))
                .await
                .context(JoinRecvFdsSnafu)?
                .context(ReceiveFdsSnafu)?;

        snafu::ensure!(fds.len() >= 2, UnexpectedFdCountSnafu { actual: fds.len() });

        let mut fds = fds.into_iter();
        let stdin_fd = fds.next().expect("checked len >= 2");
        let stdout_fd = fds.next().expect("checked len >= 2");

        Ok(gateway::control_plane::SessionTransport {
            stdin: stdin_fd,
            stdout: stdout_fd,
        })
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

impl gateway::control_plane::ProvideListener for RemoteControlPlane {
    type Listener = RemoteListener;
    type ListenError = RemoteListenError;

    async fn listener(&self, request: ListenRequest) -> Result<Self::Listener, Self::ListenError> {
        Ok(self.client.listener(request).await?)
    }
}

impl gateway::control_plane::ProvideConnector for RemoteControlPlane {
    type Connector = RemoteConnector;
    type ConnectError = RemoteConnectError;

    async fn connector(
        &self,
        request: ConnectorRequest,
    ) -> Result<Self::Connector, Self::ConnectError> {
        Ok(self.client.connector(request).await?)
    }
}
