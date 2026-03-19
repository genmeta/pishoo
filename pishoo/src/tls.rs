use std::sync::{Arc, Once, OnceLock};

use gm_quic::prelude::handy::ToCertificate;
use snafu::Snafu;

pub const MAX_CERT_PEM_BYTES: usize = 512 * 1024;
pub const MAX_KEY_PEM_BYTES: usize = 64 * 1024;

#[derive(Debug, Snafu)]
pub enum TlsMaterialError {
    #[snafu(display("certificate pem is too large ({actual} > {limit})"))]
    CertTooLarge { actual: usize, limit: usize },
    #[snafu(display("private key pem is too large ({actual} > {limit})"))]
    KeyTooLarge { actual: usize, limit: usize },
    #[snafu(display("invalid certificate pem"))]
    InvalidCertificatePem { source: std::io::Error },
    #[snafu(display("certificate pem contains no certificate"))]
    EmptyCertificate,
    #[snafu(display("invalid private key pem"))]
    InvalidPrivateKeyPem { source: std::io::Error },
    #[snafu(display("private key pem contains no key"))]
    EmptyPrivateKey,
}

fn install_crypto_provider() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

pub fn root_cert_store() -> Arc<rustls::RootCertStore> {
    static ROOT_CERT_STORE: OnceLock<Arc<rustls::RootCertStore>> = OnceLock::new();

    install_crypto_provider();

    ROOT_CERT_STORE
        .get_or_init(|| {
            let mut store = rustls::RootCertStore::empty();
            let root_cert = include_bytes!("../../keychain/root.crt");
            store.add_parsable_certificates(root_cert.to_certificate());
            Arc::new(store)
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_cert_store_loads_certificates() {
        let store = root_cert_store();
        assert!(
            !store.is_empty(),
            "shared root cert store must load certificates"
        );
    }
}
