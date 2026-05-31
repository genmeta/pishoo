use std::{
    io::Cursor,
    path::{Path, PathBuf},
    sync::Arc,
};

use http::Uri;
use rustls::{
    ClientConfig, RootCertStore,
    pki_types::{PrivateKeyDer, ServerName},
};
use rustls_pemfile::{certs, private_key};
use snafu::{ResultExt, ensure_whatever, whatever};
use tokio::net::TcpStream;
use tokio_rustls::{TlsConnector, client::TlsStream};
use tracing::debug;

use crate::{
    error::Whatever,
    parse::{document::ConfigNode, types::PathConfig},
};

type Result<T, E = Whatever> = std::result::Result<T, E>;

#[derive(Debug, Clone, Default)]
struct UpstreamTlsSettings {
    client_cert: Option<PathBuf>,
    client_key: Option<PathBuf>,
    trusted_ca: Option<PathBuf>,
}

impl UpstreamTlsSettings {
    /// 从 location 配置节点中提取上游 TLS 所需的证书路径。
    fn from_location(location: &ConfigNode) -> Self {
        Self {
            client_cert: path_from_config(location, "proxy_ssl_certificate"),
            client_key: path_from_config(location, "proxy_ssl_certificate_key"),
            trusted_ca: path_from_config(location, "proxy_ssl_trusted_certificate"),
        }
    }
}

/// 连接 HTTPS 上游，并在 TCP 连接之上完成 TLS 握手。
pub(super) async fn connect_https(
    location: &ConfigNode,
    proxy_pass: &Uri,
) -> Result<TlsStream<TcpStream>> {
    // 先从 proxy_pass 中解析服务端主机名和端口，作为 TCP 连接与 SNI 的输入。
    let host = proxy_pass.host().expect("missing host in proxy_pass uri");
    let port = proxy_pass.port_u16().unwrap_or(443);
    let server_name = ServerName::try_from(host.to_string())
        .whatever_context::<_, Whatever>(format!("invalid upstream tls server name `{host}`"))?;

    debug!(host, port, "connecting to https upstream");

    let tcp_stream = TcpStream::connect((host, port))
        .await
        .whatever_context::<_, Whatever>(format!(
            "cannot connect to https upstream {host}:{port}"
        ))?;

    // 根据 location 中的 TLS 配置构造客户端配置，再执行 TLS 握手。
    let tls_config = build_client_config(location)?;
    let connector = TlsConnector::from(tls_config);

    let tls_stream = connector
        .connect(server_name, tcp_stream)
        .await
        .whatever_context::<_, Whatever>(format!(
            "failed to establish tls connection to upstream {host}:{port}"
        ))?;

    Ok(tls_stream)
}

/// 构建上游 TLS 客户端配置，支持自定义 CA 和可选的双向 TLS 客户端证书。
fn build_client_config(location: &ConfigNode) -> Result<Arc<ClientConfig>> {
    let settings = UpstreamTlsSettings::from_location(location);

    // 客户端证书和私钥必须成对出现，否则无法完成双向 TLS 配置。
    ensure_whatever!(
        settings.client_cert.is_some() == settings.client_key.is_some(),
        "proxy_ssl_certificate and proxy_ssl_certificate_key must be configured together"
    );

    debug!(
        client_auth_enabled = settings.client_cert.is_some(),
        custom_trusted_ca = settings.trusted_ca.is_some(),
        "building upstream tls client config"
    );

    let root_store = build_root_store(settings.trusted_ca.as_deref())?;
    let builder = ClientConfig::builder().with_root_certificates(root_store);

    // 如果显式配置了客户端证书和私钥，则开启双向 TLS；否则退回到单向校验。
    let mut config = if let (Some(cert_path), Some(key_path)) = (
        settings.client_cert.as_deref(),
        settings.client_key.as_deref(),
    ) {
        let cert_chain = load_cert_chain(cert_path, "upstream client certificate")?;
        let private_key = load_private_key(key_path)?;

        builder
            .with_client_auth_cert(cert_chain, private_key)
            .whatever_context::<_, Whatever>(format!(
                "failed to configure upstream tls client identity from `{}` and `{}`",
                cert_path.display(),
                key_path.display()
            ))?
    } else {
        builder.with_no_client_auth()
    };

    config.enable_sni = true;
    Ok(Arc::new(config))
}

/// 构建根证书池：默认使用系统/项目内置根证书，并按需追加自定义上游 CA。
fn build_root_store(trusted_ca: Option<&Path>) -> Result<RootCertStore> {
    let mut root_store = dhttp::trust::dhttp_root_cert_store().as_ref().clone();

    if let Some(ca_path) = trusted_ca {
        let trusted_certs = load_cert_chain(ca_path, "upstream trusted ca certificate")?;

        // 自定义 CA 证书链中的每一张证书都加入根证书池，供后续校验上游证书使用。
        for cert in trusted_certs {
            root_store
                .add(cert)
                .whatever_context::<_, Whatever>(format!(
                    "failed to add upstream trusted ca certificate from `{}`",
                    ca_path.display()
                ))?;
        }
    }

    Ok(root_store)
}

/// 从 PEM 文件中读取证书链，并校验文件中至少包含一张证书。
fn load_cert_chain(
    path: &Path,
    label: &str,
) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    let cert_bytes = std::fs::read(path).whatever_context::<_, Whatever>(format!(
        "failed to read {label} from `{}`",
        path.display()
    ))?;

    let cert_chain = certs(&mut Cursor::new(cert_bytes))
        .collect::<std::result::Result<Vec<_>, _>>()
        .whatever_context::<_, Whatever>(format!(
            "failed to parse {label} from `{}`",
            path.display()
        ))?;

    ensure_whatever!(
        !cert_chain.is_empty(),
        "no certificates found in {} `{}`",
        label,
        path.display()
    );

    Ok(cert_chain)
}

/// 从 PEM 文件中读取私钥，用于配置上游双向 TLS 的客户端身份。
fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>> {
    let key_bytes = std::fs::read(path).whatever_context::<_, Whatever>(format!(
        "failed to read upstream client private key from `{}`",
        path.display()
    ))?;

    let mut cursor = Cursor::new(key_bytes);
    let parsed_key = private_key(&mut cursor)
        .whatever_context::<_, Whatever>(format!(
            "failed to parse upstream client private key from `{}`",
            path.display()
        ))?
        .map(|private_key| private_key.clone_key());

    let Some(private_key) = parsed_key else {
        whatever!("no private key found in `{}`", path.display());
    };

    Ok(private_key)
}

fn path_from_config(location: &ConfigNode, name: &str) -> Option<PathBuf> {
    location
        .get::<PathConfig>(name)
        .ok()
        .flatten()
        .map(|path| path.0.clone())
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Once};

    use super::*;
    use crate::parse::{
        registry::context,
        source::{SourceId, SourceSpan},
        value::TypedValue,
    };

    static INSTALL_CRYPTO_PROVIDER: Once = Once::new();

    fn ensure_crypto_provider() {
        INSTALL_CRYPTO_PROVIDER.call_once(|| {
            rustls::crypto::ring::default_provider()
                .install_default()
                .expect("ring crypto provider should install once");
        });
    }

    fn fixture_path(relative: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join(relative)
    }

    fn test_location(values: &[(&'static str, PathBuf)]) -> ConfigNode {
        let span = SourceSpan::new(SourceId(0), 0, 0);
        let mut location = ConfigNode::new(context::LOCATION, None, span);
        for (name, path) in values {
            location.insert_slot(name, TypedValue::new(PathConfig(path.clone()), span));
        }
        location
    }

    #[test]
    fn test_load_cert_chain_from_existing_fixture() {
        let cert_path = fixture_path("keychain/test.genmeta.net/test.genmeta.net.pem");

        let certs = load_cert_chain(&cert_path, "test fixture certificate")
            .expect("fixture certificate should load");

        assert!(!certs.is_empty());
    }

    #[test]
    fn test_load_private_key_from_existing_fixture() {
        let key_path = fixture_path("keychain/test.genmeta.net/test.genmeta.net.key");

        let key = load_private_key(&key_path).expect("fixture private key should load");

        assert!(!key.secret_der().is_empty());
    }

    #[test]
    fn test_build_client_config_with_trusted_ca_only() {
        ensure_crypto_provider();

        let trusted_ca = fixture_path("keychain/root.crt");
        let location = test_location(&[("proxy_ssl_trusted_certificate", trusted_ca)]);
        let client_config = build_client_config(&location).expect("client config should build");

        assert!(client_config.enable_sni);
    }

    #[test]
    fn test_build_client_config_with_client_identity() {
        ensure_crypto_provider();

        let trusted_ca = fixture_path("keychain/root.crt");
        let client_cert = fixture_path("keychain/test.genmeta.net/test.genmeta.net.pem");
        let client_key = fixture_path("keychain/test.genmeta.net/test.genmeta.net.key");
        let location = test_location(&[
            ("proxy_ssl_trusted_certificate", trusted_ca),
            ("proxy_ssl_certificate", client_cert),
            ("proxy_ssl_certificate_key", client_key),
        ]);
        let client_config = build_client_config(&location).expect("client config should build");

        assert!(client_config.enable_sni);
    }

    #[test]
    fn test_build_client_config_rejects_incomplete_identity() {
        let client_cert = fixture_path("keychain/test.genmeta.net/test.genmeta.net.pem");
        let location = test_location(&[("proxy_ssl_certificate", client_cert)]);
        let error = build_client_config(&location).expect_err("incomplete identity should fail");

        assert!(error.to_string().contains("must be configured together"));
    }
}
