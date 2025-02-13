use std::sync::{Arc, OnceLock};

use rustls::RootCertStore;
use tracing::error;
use webpki::types::{CertificateDer, pem::PemObject};

/// 初始化服务
pub async fn init() {
    // 初始化日志
    tracing();
    // 初始化TLS
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// 初始化日志
fn tracing() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_file(true)
        .with_line_number(true)
        .with_ansi(false)
        .init();
    tracing::info!("Tracing initialized.");
}

pub fn root_cert() -> Arc<RootCertStore> {
    static ROOT_CERT_STORE: OnceLock<Arc<RootCertStore>> = OnceLock::new();
    let root_cert = include_bytes!("../root.crt");
    ROOT_CERT_STORE
        .get_or_init(|| {
            let mut root_cert_store = rustls::RootCertStore::empty();
            for cert in rustls_native_certs::load_native_certs().certs {
                if let Err(error) = root_cert_store.add(cert) {
                    error!("failed to parse trust anchor {error}");
                }
            }

            let root_cert = match CertificateDer::from_pem_slice(root_cert) {
                Ok(root) => vec![root],
                Err(_) => vec![CertificateDer::from(root_cert.to_vec())],
            };

            root_cert_store.add_parsable_certificates(root_cert);
            Arc::new(root_cert_store)
        })
        .clone()
}
