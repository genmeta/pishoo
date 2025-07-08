use std::sync::Arc;

use bytes::Bytes;
use dashmap::DashMap;
use gm_quic::{EndpointAddr, QuicClient};
use h3::client::SendRequest;
use tokio::{io, sync::Mutex};

#[derive(Clone)]
pub struct ReusableConnection {
    #[allow(unused)]
    pub quic: Arc<gm_quic::Connection>,
    pub h3: SendRequest<h3_shim::OpenStreams, Bytes>,
}

/// H3 Connection reuse pool
pub struct H3ConnectionPool {
    quic_client: Arc<QuicClient>,
    h3_clients: Arc<DashMap<String, Arc<Mutex<Option<ReusableConnection>>>>>,
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
        server_eps: impl IntoIterator<Item = impl Into<EndpointAddr>>,
    ) -> io::Result<ReusableConnection> {
        let server_name = server_name.into();

        let mut entry = None;

        // Get a shared access so that multiple asynchronous tasks can asynchronously wait for other tasks
        // to create connections
        let entry = loop {
            match entry {
                Some(entry) => break entry,
                None => {
                    self.h3_clients.entry(server_name.clone()).or_default();
                    entry = self.h3_clients.get(&server_name).map(|e| e.clone());
                }
            }
        };

        let mut entry = entry.lock().await;

        if let Some(conn) = entry.as_ref() {
            // todo: fresh quic conenc
            tracing::debug!("[pool] Reusing connection to {server_name}");
            return io::Result::Ok(conn.clone());
        }

        let connect_or_reuse = async {
            let quic_connection = self.quic_client.connect(server_name.clone(), server_eps)?;
            let (mut h3_connection, send_request) =
                h3::client::new(h3_shim::QuicConnection::new(quic_connection.clone()))
                    .await
                    .map_err(io::Error::other)?;

            let conn = ReusableConnection {
                quic: quic_connection.clone(),
                h3: send_request.clone(),
            };

            *entry = Some(conn.clone());

            tokio::spawn({
                let h3_clients = self.h3_clients.clone();
                let server_name = server_name.clone();
                async move {
                    _ = h3_connection.wait_idle().await;
                    h3_clients.remove(&server_name);
                }
            });

            tracing::debug!("[Pool] Created connection to {server_name}");

            Ok(conn)
        };

        match connect_or_reuse.await {
            Ok(send_request) => Ok(send_request),
            Err(error) => {
                // clean up failed connections
                tracing::debug!("[Pool] Failed to connect to {server_name}: {error}");
                tokio::task::spawn_blocking({
                    let h3_clients = self.h3_clients.clone();
                    move || h3_clients.remove_if(&server_name, |_, v| v.blocking_lock().is_none())
                });
                Err(error)
            }
        }
    }
}
