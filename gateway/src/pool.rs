use std::{
    error::Error,
    fmt::Display,
    sync::{Arc, RwLock},
};

use bytes::Bytes;
use dashmap::DashMap;
use futures::{Stream, StreamExt, future, stream};
use gm_quic::{QuicClient, SocketEndpointAddr};
use h3::client::SendRequest;
use qdns::Resolvers;
use snafu::{OptionExt, Report, ResultExt};
use tokio::{io, sync::Mutex};
use tracing::debug;

use crate::{error::Whatever, forward::create_quic_client};

#[derive(Debug)]
pub struct DnsErrors {
    errors: Vec<(String, io::Error)>,
}

impl Display for DnsErrors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (source, error) in &self.errors {
            writeln!(
                f,
                "Resolver `{source}` failed: {}",
                Report::from_error(error)
            )?;
        }
        Ok(())
    }
}

impl Error for DnsErrors {}

pub async fn lookup(
    resolvers: &Resolvers,
    server_name: &str,
) -> Result<impl Stream<Item = Vec<SocketEndpointAddr>> + use<>, DnsErrors> {
    let mut errors = vec![];

    let mut lookup_stream = resolvers.lookup(server_name);
    let endpoints = loop {
        match lookup_stream.next().await {
            // 多余？
            Some((source, Ok(endpoints))) if endpoints.is_empty() => {
                errors.push((
                    source,
                    io::Error::new(io::ErrorKind::NotFound, "No endpoints addresses found"),
                ));
            }
            Some((source, Err(error))) => {
                errors.push((source, error));
            }
            Some((_source, Ok(endpoints))) => break endpoints,
            None => return Err(DnsErrors { errors }),
        }
    };
    Ok(stream::once(future::ready(endpoints))
        .chain(lookup_stream.filter_map(|(_, endpoints)| future::ready(endpoints.ok()))))
}

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
    /// 获取全局连接池实例
    pub fn global() -> Arc<Self> {
        static GLOBAL: RwLock<Option<Arc<H3ConnectionPool>>> = RwLock::new(None);
        if let Ok(guard) = GLOBAL.read()
            && let Some(pool) = guard.as_ref()
        {
            return pool.clone();
        }
        let mut guard = GLOBAL.write().unwrap();
        if let Some(pool) = guard.as_ref() {
            return pool.clone();
        }
        let pool = Arc::new(H3ConnectionPool::new());
        *guard = Some(pool.clone());
        pool
    }

    /// 重新初始化全局连接池
    pub fn reinitialize() -> Arc<Self> {
        static GLOBAL: RwLock<Option<Arc<H3ConnectionPool>>> = RwLock::new(None);
        debug!(target: "pool", "Reinitializing H3ConnectionPool");
        let mut guard = GLOBAL.write().unwrap();
        let pool = Arc::new(H3ConnectionPool::new());
        *guard = Some(pool.clone());
        pool
    }
    /// Creates a new reuse pool, using the given client to create the underlying quic connection.
    ///
    /// If this client is used by multiple [`H3ConnectionPool`] and the client enables [`reuse_connection`], it may cause some problems.
    ///
    /// [`reuse_connection`]: gm_quic::QuicClientBuilder::reuse_connection
    pub fn new() -> Self {
        Self {
            quic_client: Arc::new(create_quic_client()),
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
        resolvers: Resolvers,
    ) -> Result<ReusableConnection, Whatever> {
        let server_name: String = server_name.into();
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
            tracing::debug!(target: "pool", "Reusing connection to {server_name}");
            return Ok(conn.clone());
        }

        let connect_or_reuse = async {
            let mut lookup = lookup(&resolvers, &server_name)
                .await
                .whatever_context("DNS lookup failed")?;

            let server_eps = lookup
                .next()
                .await
                .whatever_context("No endpoint found for server")?;

            let quic_connection = self
                .quic_client
                .connect(server_name.clone(), server_eps)
                .unwrap();

            tokio::spawn({
                let conn = quic_connection.clone();
                async move {
                    while let Some(endpoints) = lookup.next().await {
                        for endpoint in endpoints {
                            _ = conn.add_peer_endpoint(endpoint.into());
                        }
                    }
                }
            });
            let (mut h3_connection, send_request) =
                h3::client::new(h3_shim::QuicConnection::new(quic_connection.clone()))
                    .await
                    .whatever_context("Failed to establish h3 connection")?;

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
                tracing::debug!(
                    "[Pool] Failed to connect to {server_name}: {}",
                    Report::from_error(&error)
                );
                tokio::task::spawn_blocking({
                    let h3_clients = self.h3_clients.clone();
                    move || h3_clients.remove_if(&server_name, |_, v| v.blocking_lock().is_none())
                });
                Err(error)
            }
        }
    }

    pub fn clear_connections(&self) {
        self.h3_clients.clear();
    }
}
