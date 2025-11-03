use std::{
    sync::{Arc, RwLock},
    time::Duration,
};

use bytes::Bytes;
use dashmap::DashMap;
use futures::StreamExt;
use gm_quic::{
    prelude::{
        Connection, ParameterId, QuicClient,
        handy::{ToCertificate, ToPrivateKey, client_parameters},
    },
    qinterface::iface::{
        QuicInterfaces,
        physical::{InterfaceEvent, PhysicalInterfaces},
    },
};
use h3::client::SendRequest;
use qdns::Resolvers;
use snafu::{OptionExt, Report, ResultExt};
use tokio::{sync::Mutex, time};
use tokio_util::task::AbortOnDropHandle;
use tracing::debug;

use crate::{
    error::Whatever,
    parse::{IfaceRange, IpFamilies, Listens},
    traversal_factory,
};

/// 客户端配置类型: (证书链, 私钥, 客户端名称)
type ClientConfig = (Vec<u8>, Vec<u8>, String);

/// 全局客户端配置存储
static CLIENT_CONFIG: RwLock<Option<ClientConfig>> = RwLock::new(None);

/// 设置客户端配置
///
/// 可以多次调用以更新证书、密钥和客户端名称。
pub fn set_client_config(
    cert_chain: Vec<u8>,
    private_key: Vec<u8>,
    client_name: String,
) -> Result<(), &'static str> {
    let mut config = CLIENT_CONFIG
        .write()
        .map_err(|_| "Failed to acquire write lock")?;
    *config = Some((cert_chain, private_key, client_name));
    Ok(())
}

/// 获取客户端配置
fn get_client_config() -> Option<ClientConfig> {
    CLIENT_CONFIG.read().ok().and_then(|guard| guard.clone())
}

#[derive(Clone)]
pub struct ReusableConnection {
    #[allow(unused)]
    pub quic: Arc<Connection>,
    pub h3: SendRequest<h3_shim::OpenStreams, Bytes>,
}

/// H3 Connection reuse pool
pub struct H3ConnectionPool {
    quic_client: Arc<QuicClient>,
    _maintain_binding: AbortOnDropHandle<()>,
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
        let config = get_client_config();
        let pool = Arc::new(H3ConnectionPool::new(config));
        *guard = Some(pool.clone());
        pool
    }

    /// 重新初始化全局连接池
    pub fn reinitialize() -> Arc<Self> {
        static GLOBAL: RwLock<Option<Arc<H3ConnectionPool>>> = RwLock::new(None);
        debug!(target: "pool", "Reinitializing H3ConnectionPool");
        let mut guard = GLOBAL.write().unwrap();
        let config = get_client_config();
        let pool = Arc::new(H3ConnectionPool::new(config));
        *guard = Some(pool.clone());
        pool
    }
    /// Creates a new reuse pool, using the given client to create the underlying quic connection.
    ///
    /// If this client is used by multiple [`H3ConnectionPool`] and the client enables [`reuse_connection`], it may cause some problems.
    ///
    /// # Arguments
    /// * `config` - 可选的配置元组 (cert_chain, key_der, client_name)
    ///   - 如果为 None，则不使用客户端认证
    ///   - cert_chain: PEM 或 DER 格式的证书链
    ///   - key_der: PEM 或 DER 格式的私钥
    ///   - client_name: 客户端名称
    ///
    /// [`reuse_connection`]: gm_quic::QuicClientBuilder::reuse_connection
    pub fn new(config: Option<ClientConfig>) -> Self {
        let provider = Arc::new(rustls::crypto::ring::default_provider());

        // 创建基础 TLS 配置 builder
        let tls_builder = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_root_certificates(crate::common::root_cert());

        // 根据是否有客户端配置选择认证方式
        let mut tls_config = if let Some((cert_chain, key_der, _)) = config.as_ref() {
            // 使用客户端认证
            tls_builder
                .with_client_auth_cert(cert_chain.to_certificate(), key_der.to_private_key())
                .unwrap()
        } else {
            // 不使用客户端认证
            tls_builder.with_no_client_auth()
        };

        // TLS 特性配置
        tls_config.resumption = rustls::client::Resumption::disabled();
        tls_config.key_log = Arc::new(rustls::KeyLogFile::new());

        #[cfg_attr(not(feature = "qlog"), allow(unused_mut))]
        let mut builder = QuicClient::builder_with_tls(tls_config)
            .enable_sslkeylog()
            .with_iface_factory(traversal_factory().as_ref().clone());
        // .with_alpns([ALPN]);

        #[cfg(feature = "qlog")]
        {
            use std::path::PathBuf;

            use qevent::telemetry::handy::DefaultSeqLogger;

            builder =
                builder.with_qlog(Arc::new(DefaultSeqLogger::new(PathBuf::from("/tmp/qlog"))));
        }

        let mut monitor = PhysicalInterfaces::global().monitor();

        let listen_all = Listens::new(IfaceRange::All, IpFamilies::Dual, 0);

        let mut parameters = client_parameters();
        // 仅在提供了 client_name 时设置
        if let Some((_, _, client_name)) = config.as_ref() {
            parameters
                .set(ParameterId::ClientName, client_name.clone())
                .unwrap();
        }

        let client = Arc::new(
            builder
                .with_parameters(parameters)
                .defer_idle_timeout(Duration::from_secs(60))
                .bind(listen_all.resolve(monitor.interfaces().keys().map(|d| d.as_str())))
                .build(),
        );

        let quic_client = client.clone();
        let maintain_binding = AbortOnDropHandle::new(tokio::spawn(async move {
            while let Some((_currnet_interfaces, event)) = monitor.update().await {
                tracing::debug!(target: "listen", ?event, "Interface event received");
                match event.as_ref() {
                    InterfaceEvent::Added { device, .. } => {
                        for bind_uri in listen_all.resolve([device.as_str()]) {
                            debug!(target: "listen", ?bind_uri, "Add interface to client binding");
                            let bind_interface = QuicInterfaces::global()
                                .bind(bind_uri, traversal_factory().clone());
                            quic_client.add_interface(bind_interface);
                        }
                    }
                    InterfaceEvent::Removed { device, .. } => {
                        for bind_uri in listen_all.resolve([device.as_str()]) {
                            debug!(target: "listen", ?bind_uri, "Remove interface from client binding");
                            quic_client.remove_interface(&bind_uri);
                        }
                    }
                    InterfaceEvent::Changed { .. } => { /* Ignore changes */ }
                }
            }
        }));

        Self {
            quic_client: client,
            _maintain_binding: maintain_binding,
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
        server_endpoints: Resolvers,
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
            let mut lookup = server_endpoints
                .lookup(&server_name)
                .await
                .whatever_context("DNS lookup failed")?;

            let (_resolver, server_eps) = lookup
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
                    while let Some((_resolver, endpoints)) = lookup.next().await {
                        for endpoint in endpoints {
                            _ = conn.add_peer_endpoint(endpoint.into());
                        }
                    }
                }
            });
            let connect = h3::client::new(h3_shim::QuicConnection::new(quic_connection.clone()));
            let (mut h3_connection, send_request) = time::timeout(Duration::from_secs(10), connect)
                .await
                .whatever_context("Establish h3 connection timed out")?
                .whatever_context("Failed to establish HTTP3 connection")?;

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
