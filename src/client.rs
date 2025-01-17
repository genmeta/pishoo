use reqwest::Client;
use rustls::RootCertStore;
use std::{
    net::IpAddr,
    sync::{Arc, OnceLock},
};
use tracing::error;
use webpki::types::{CertificateDer, pem::PemObject};

fn root_cert() -> Arc<RootCertStore> {
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

pub(crate) fn launch_h3_client(addr: IpAddr) -> Client {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls_config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_root_certificates(root_cert())
        .with_no_client_auth();

    tls_config.alpn_protocols.push(b"h3".into());

    reqwest::Client::builder()
        .local_address(addr)
        .use_preconfigured_tls(tls_config)
        .build()
        .unwrap()
}
