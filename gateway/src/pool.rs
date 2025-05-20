use std::sync::Arc;

use bytes::Bytes;
use dashmap::DashMap;
use gm_quic::QuicClient;
use h3::client::SendRequest;
use h3_shim::{OpenStreams, QuicConnection};
use qconnection::prelude::ToEndpointAddr;
use tokio::{io, sync::Notify};

#[derive(Clone)]
pub struct ReusableConnection {
    #[allow(unused)]
    pub quic: Arc<gm_quic::Connection>,
    pub h3: SendRequest<OpenStreams, Bytes>,
}

enum ConnectionState {
    Connecting(Arc<Notify>),
    Connected(ReusableConnection),
}

/// H3 Connection reuse pool
pub struct H3ConnectionPool {
    quic_client: Arc<QuicClient>,
    h3_clients: Arc<DashMap<String, ConnectionState>>,
}

impl H3ConnectionPool {
    /// Creates a new reuse pool, using the given client to create the underlying quic connection.
    ///
    /// If this client is used by multiple [`H3ConnectionPool`] and the client enables [`reuse_connection`], it may cause some problems.
    ///
    /// [`reuse_connection`]: gm_quic::QuicClientBuilder::reuse_connection
    pub fn new(quic_client: Arc<QuicClient>) -> Self {
        Self {
            quic_client,
            h3_clients: Arc::new(DashMap::new()),
        }
    }

    /// Get a connection to the specified server.
    ///
    /// If there is no current connection to the server, the given endpoint addr will be used to create a connection.
    ///
    /// If there is already a connection to the given server, just return the existing connection.
    pub async fn connect(
        &self,
        server_name: impl Into<String>,
        server_ep: impl ToEndpointAddr,
    ) -> io::Result<ReusableConnection> {
        let server_name = server_name.into();

        loop {
            let notify = match self.h3_clients.get(&server_name) {
                Some(entry) => match entry.value() {
                    ConnectionState::Connected(conn) => {
                        tracing::debug!("[Pool] Reusing existing connection for {}", server_name);
                        return io::Result::Ok(conn.clone());
                    }
                    ConnectionState::Connecting(notify) => {
                        tracing::debug!("[Pool] Waiting for connection for {}", server_name);
                        Arc::clone(notify)
                    }
                },
                None => {
                    tracing::debug!("[Pool] Connecting to {}", server_name);
                    let notify = Notify::new();
                    self.h3_clients.insert(
                        server_name.clone(),
                        ConnectionState::Connecting(Arc::new(notify)),
                    );
                    break;
                }
            };
            notify.notified().await;
        }

        let connect_or_reuse = async {
            tracing::info!("[Pool] Creating new connection for {}", server_name);

            let quic_connection = self.quic_client.connect(server_name.clone(), server_ep)?;
            let (mut h3_connection, send_request) =
                h3::client::new(QuicConnection::new(quic_connection.clone()).await)
                    .await
                    .map_err(io::Error::other)?;

            let conn = ReusableConnection {
                quic: quic_connection.clone(),
                h3: send_request.clone(),
            };

            tokio::spawn({
                let h3_clients = self.h3_clients.clone();
                let server_name = server_name.clone();
                async move {
                    _ = h3_connection.wait_idle().await;
                    h3_clients.remove(&server_name);
                }
            });

            let state = self.h3_clients.insert(
                server_name.clone(),
                ConnectionState::Connected(conn.clone()),
            );
            if let Some(ConnectionState::Connecting(notify)) = state {
                notify.notify_waiters();
            }
            Ok(conn)
        };

        match connect_or_reuse.await {
            Ok(conn) => Ok(conn),
            Err(error) => {
                // clean up failed connections
                tokio::task::spawn_blocking({
                    let h3_clients = self.h3_clients.clone();
                    move || {
                        h3_clients.remove_if(&server_name, |_, v| {
                            matches!(v, ConnectionState::Connecting(_))
                        })
                    }
                });
                Err(error)
            }
        }
    }
}
