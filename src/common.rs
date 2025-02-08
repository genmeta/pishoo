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
        .with_max_level(tracing::Level::INFO)
        .with_file(true)
        .with_line_number(true)
        .init();
    tracing::info!("Tracing initialized.");
}

pub fn root_cert() -> Arc<RootCertStore> {
    static ROOT_CERT_STORE: OnceLock<Arc<RootCertStore>> = OnceLock::new();
    ROOT_CERT_STORE
        .get_or_init(|| {
            let root_file = "root.crt";
            let mut root_cert_store = rustls::RootCertStore::empty();
            for cert in rustls_native_certs::load_native_certs().certs {
                if let Err(error) = root_cert_store.add(cert) {
                    error!("failed to parse trust anchor {error}");
                }
            }
            let root = std::fs::read(root_file)
                .unwrap_or_else(|_| panic!("failed to read root certificate file {}", root_file));
            let root_cert = match CertificateDer::from_pem_slice(&root) {
                Ok(root_cert) => vec![root_cert],
                Err(_) => vec![CertificateDer::from(root)],
            };

            root_cert_store.add_parsable_certificates(root_cert);
            Arc::new(root_cert_store)
        })
        .clone()
}
