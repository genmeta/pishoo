use std::sync::{Arc, OnceLock};

use gm_quic::prelude::handy::ToCertificate;
use rustls::RootCertStore;

pub fn root_cert() -> Arc<RootCertStore> {
    static ROOT_CERT_STORE: OnceLock<Arc<RootCertStore>> = OnceLock::new();
    let root_cert = include_bytes!("../../keychain/root.crt");

    ROOT_CERT_STORE
        .get_or_init(|| {
            let mut root_cert_store = rustls::RootCertStore::empty();
            root_cert_store.add_parsable_certificates(root_cert.to_certificate());
            Arc::new(root_cert_store)
        })
        .clone()
}
