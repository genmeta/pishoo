use std::sync::{Arc, RwLock};

use gmdns::resolvers::Resolvers;
use h3x::dquic::{
    H3Client,
    prelude::{ParameterId, handy::client_parameters},
    qinterface::{device::Devices, io::ProductIO, manager::InterfaceManager},
};
use snafu::Snafu;
use tracing::debug;

use crate::parse::{IfaceRange, IpFamilies, Listens};

/// 客户端配置类型: (证书链, 私钥, 客户端名称)
type ClientConfig = (Vec<u8>, Vec<u8>, String);

#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum SetClientConfigError {
    #[snafu(display("failed to acquire h3 client config write lock"))]
    WriteLockPoisoned,
}

/// 全局客户端配置存储
static CLIENT_CONFIG: RwLock<Option<ClientConfig>> = RwLock::new(None);

/// 设置客户端配置
///
/// 可以多次调用以更新证书、密钥和客户端名称。
pub fn set_client_config(
    cert_chain: Vec<u8>,
    private_key: Vec<u8>,
    client_name: String,
) -> Result<(), SetClientConfigError> {
    let mut config = CLIENT_CONFIG
        .write()
        .map_err(|_| SetClientConfigError::WriteLockPoisoned)?;
    *config = Some((cert_chain, private_key, client_name));
    Ok(())
}

/// 获取客户端配置
fn get_client_config() -> Option<ClientConfig> {
    CLIENT_CONFIG.read().ok().and_then(|guard| guard.clone())
}

/// 全局 H3Client 实例
static GLOBAL_CLIENT: RwLock<Option<Arc<H3Client>>> = RwLock::new(None);

/// 获取全局 H3Client 实例
pub async fn global() -> Arc<H3Client> {
    if let Ok(guard) = GLOBAL_CLIENT.read()
        && let Some(client) = guard.as_ref()
    {
        return client.clone();
    }
    let client: Arc<H3Client> = Arc::new(build_client(get_client_config(), None).await);
    let Ok(mut guard) = GLOBAL_CLIENT.write() else {
        return client;
    };
    if let Some(existing) = guard.as_ref() {
        return existing.clone();
    }
    *guard = Some(client.clone());
    client
}

/// 重新初始化全局 H3Client
pub async fn reinitialize(resolver: Option<Arc<Resolvers>>) -> Arc<H3Client> {
    debug!("reinitializing h3 client");
    let client: Arc<H3Client> = Arc::new(build_client(get_client_config(), resolver).await);
    let Ok(mut guard) = GLOBAL_CLIENT.write() else {
        return client;
    };
    *guard = Some(client.clone());
    client
}

/// 构建 H3Client
///
/// 整合 TLS 配置、客户端认证、resolver、网卡绑定等。
async fn build_client(config: Option<ClientConfig>, resolver: Option<Arc<Resolvers>>) -> H3Client {
    let root_store = crate::common::root_cert();

    let tls_builder = H3Client::builder()
        .with_dangerous_server_cert_verifier(crate::ocsp::ocsp_server_cert_verifier(root_store));

    let mut builder = if let Some((ref cert_chain, ref key_der, ref name)) = config {
        tls_builder
            .with_identity(
                name.clone(),
                cert_chain.as_slice(),
                key_der.as_slice(),
            )
            .expect("failed to create client builder with identity")
    } else {
        tls_builder
            .without_identity()
            .expect("failed to create client builder")
    };

    let iface_factory: Arc<dyn ProductIO> = Arc::new(h3x::dquic::prelude::handy::DEFAULT_IO_FACTORY);
    let iface_manager = InterfaceManager::global().clone();
    let monitor = Devices::global().monitor();

    for i in monitor.interfaces() {
        debug!(interface = ?i.0, "initial interface detected");
    }

    let listen_all = Listens::new(IfaceRange::All, IpFamilies::Dual, 0);

    builder = builder
        .with_iface_factory(iface_factory)
        .with_iface_manager(iface_manager)
        .enable_sslkeylog()
        .defer_idle_timeout(std::time::Duration::from_secs(60));

    if let Some(resolver) = resolver {
        builder = builder.with_resolver(resolver);
    }

    if let Some((_, _, ref client_name)) = config {
        let mut parameters = client_parameters();
        parameters
            .set(ParameterId::ClientName, client_name.clone())
            .unwrap();
        builder = builder.with_quic_parameters(parameters);
    }

    #[cfg(feature = "qlog")]
    {
        use std::path::PathBuf;

        use qevent::telemetry::handy::LegacySeqLogger;

        builder = builder.with_qlog(Arc::new(LegacySeqLogger::new(PathBuf::from("/tmp/qlog"))));
    }

    builder
        .bind(listen_all.resolve(monitor.interfaces().keys().map(|d| d.as_str())))
        .await
        .build()
}
