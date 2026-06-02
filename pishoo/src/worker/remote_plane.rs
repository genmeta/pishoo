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

#[cfg(feature = "sshd")]
use std::future::IntoFuture;

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
        let rpc = client.spawn_session(username.to_owned(), expected);
        let receive = receiver.into_future();
        tokio::pin!(rpc);
        tokio::pin!(receive);

        let (actual, received) = tokio::select! {
            biased;
            receive_result = &mut receive => {
                let received = receive_result.context(ReceiveFdSnafu)?;
                let actual = rpc.await.context(RpcSnafu)?;
                (actual, received)
            }
            rpc_result = &mut rpc => {
                let actual = rpc_result.context(RpcSnafu)?;
                let received = receive.await.context(ReceiveFdSnafu)?;
                (actual, received)
            }
        };
        snafu::ensure!(actual == expected, FdIdMismatchSnafu { expected, actual });
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

#[cfg(all(test, feature = "sshd"))]
mod tests {
    use std::{os::fd::OwnedFd, sync::Arc, time::Duration};

    use gateway::control_plane::{ConnectorRequest, ListenRequest, SpawnSession};
    use h3x::ipc::transport::{FdTransfer, FdVec, MuxChannel};
    use remoc::prelude::ServerShared;
    use tokio_util::task::AbortOnDropHandle;
    use tracing::Instrument;

    use super::RemoteControlPlane;
    use crate::ipc::{
        ConnectError, ControlPlane, ControlPlaneClient, ControlPlaneServerShared, ListenError,
        RebuildListenError, SpawnSessionError,
    };

    struct AckingSpawnPlane {
        fd_transfer: FdTransfer,
    }

    impl ControlPlane for AckingSpawnPlane {
        async fn listener(
            &self,
            _request: ListenRequest,
        ) -> Result<h3x::ipc::quic::IpcListenClient, ListenError> {
            Err(ListenError::Internal {
                message: "not used by this test".to_owned(),
            })
        }

        async fn rebuild_listener(
            &self,
            _request: ListenRequest,
        ) -> Result<h3x::ipc::quic::IpcListenClient, RebuildListenError> {
            Err(RebuildListenError::Internal {
                message: "not used by this test".to_owned(),
            })
        }

        async fn connector(
            &self,
            _request: ConnectorRequest,
        ) -> Result<h3x::ipc::quic::IpcConnectClient, ConnectError> {
            Err(ConnectError::Internal {
                message: "not used by this test".to_owned(),
            })
        }

        async fn spawn_session(
            &self,
            _username: String,
            fd_id_raw: u64,
        ) -> Result<u64, SpawnSessionError> {
            let fd_id = h3x::varint::VarInt::try_from(fd_id_raw).map_err(|error| {
                SpawnSessionError::SpawnFailed {
                    reason: snafu::Report::from_error(&error).to_string(),
                }
            })?;
            let delivery = self.fd_transfer.delivery(fd_id);
            let (_held, delivered) = std::os::unix::net::UnixStream::pair().map_err(|error| {
                SpawnSessionError::SpawnFailed {
                    reason: error.to_string(),
                }
            })?;
            let mut fds = FdVec::new();
            fds.push(OwnedFd::from(delivered));
            delivery
                .deliver(fds)
                .await
                .map_err(|error| SpawnSessionError::SpawnFailed {
                    reason: snafu::Report::from_error(&error).to_string(),
                })?;
            Ok(fd_id_raw)
        }
    }

    fn mux_pair() -> (MuxChannel, MuxChannel) {
        let (left, right) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        let left = MuxChannel::from_fd(OwnedFd::from(left)).expect("left mux channel");
        let right = MuxChannel::from_fd(OwnedFd::from(right)).expect("right mux channel");
        (left, right)
    }

    #[tokio::test]
    async fn spawn_session_polls_fd_receiver_while_rpc_is_waiting_for_ack() {
        let (server_mux, client_mux) = mux_pair();
        let (server_sink, server_stream) = server_mux.split().expect("server mux split");
        let server_fd_transfer = server_stream.fd_transfer(server_sink.fd_sender());
        let (client_sink, client_stream) = client_mux.split().expect("client mux split");
        let client_fd_transfer = client_stream.fd_transfer(client_sink.fd_sender());

        let server_task = AbortOnDropHandle::new(tokio::spawn(async move {
            let (conn, mut tx, _rx) =
                remoc::Connect::framed::<_, _, ControlPlaneClient, (), remoc::codec::Default>(
                    remoc::Cfg::default(),
                    server_sink,
                    server_stream,
                )
                .await
                .expect("server remoc connection");
            let _conn_task = AbortOnDropHandle::new(tokio::spawn(conn.in_current_span()));

            let rpc_impl = AckingSpawnPlane {
                fd_transfer: server_fd_transfer,
            };
            let (server, client) = ControlPlaneServerShared::new(Arc::new(rpc_impl), 1);
            let _server_task = AbortOnDropHandle::new(tokio::spawn(
                async move {
                    let _ = server.serve(true).await;
                }
                .in_current_span(),
            ));

            tx.send(client).await.expect("send control plane client");
            futures::future::pending::<()>().await;
        }));

        let (conn, _tx, mut rx) =
            remoc::Connect::framed::<_, _, (), ControlPlaneClient, remoc::codec::Default>(
                remoc::Cfg::default(),
                client_sink,
                client_stream,
            )
            .await
            .expect("client remoc connection");
        let _conn_task = AbortOnDropHandle::new(tokio::spawn(conn.in_current_span()));
        let client = rx
            .recv()
            .await
            .expect("receive control plane client")
            .expect("control plane client sent");
        let plane = RemoteControlPlane::new(client, client_fd_transfer);

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            SpawnSession::spawn_session(&plane, "alice"),
        )
        .await
        .expect("spawn_session should poll fd receiver before rpc returns")
        .expect("spawn_session should receive delivered fd");

        drop(result);
        drop(server_task);
    }
}
