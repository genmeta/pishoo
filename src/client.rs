use reqwest::Client;
use rustls::{ClientConfig, RootCertStore};
use std::{
    net::{IpAddr, SocketAddr},
    sync::{Arc, OnceLock},
};
use tracing::error;
use webpki::types::{CertificateDer, pem::PemObject};

pub(crate) fn client(local: IpAddr, dns: Option<(&str, SocketAddr)>) -> Client {
    let mut builder = reqwest::Client::builder()
        .local_address(local)
        .use_preconfigured_tls(client_config());
    if let Some((domain, addr)) = dns {
        builder = builder.resolve(domain, addr)
    };
    builder.build().unwrap()
}

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

fn client_config() -> ClientConfig {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut client_config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_root_certificates(root_cert())
        .with_no_client_auth();

    client_config.alpn_protocols.push(b"h3".into());
    client_config
}
