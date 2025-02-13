use std::sync::{Arc, OnceLock};

use rustls::RootCertStore;
use tracing::error;
use webpki::types::{CertificateDer, pem::PemObject};

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
