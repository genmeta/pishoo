use std::sync::{Arc, Once, OnceLock};

use gm_quic::prelude::handy::ToCertificate;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
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

pub fn validate_tls_material(
    cert_pem: &[u8],
    key_pem: &[u8],
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), TlsMaterialError> {
    use std::io::Cursor;

    use snafu::{OptionExt, ResultExt};

    if cert_pem.len() > MAX_CERT_PEM_BYTES {
        return Err(TlsMaterialError::CertTooLarge {
            actual: cert_pem.len(),
            limit: MAX_CERT_PEM_BYTES,
        });
    }

    if key_pem.len() > MAX_KEY_PEM_BYTES {
        return Err(TlsMaterialError::KeyTooLarge {
            actual: key_pem.len(),
            limit: MAX_KEY_PEM_BYTES,
        });
    }

    let certs = rustls_pemfile::certs(&mut Cursor::new(cert_pem))
        .collect::<Result<Vec<_>, _>>()
        .context(InvalidCertificatePemSnafu)?;
    let _first_cert = certs.first().context(EmptyCertificateSnafu)?;

    let key = rustls_pemfile::private_key(&mut Cursor::new(key_pem))
        .context(InvalidPrivateKeyPemSnafu)?
        .context(EmptyPrivateKeySnafu)?;

    Ok((certs, key))
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

    #[test]
    fn validate_tls_material_rejects_empty_payloads() {
        let err = validate_tls_material(b"", b"").expect_err("empty pem should fail");
        assert!(matches!(
            err,
            TlsMaterialError::InvalidCertificatePem { .. } | TlsMaterialError::EmptyCertificate
        ));
    }
}
