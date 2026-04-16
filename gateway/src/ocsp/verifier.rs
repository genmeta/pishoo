use std::sync::Arc;

use der::{Decode, Encode};
use rustls::{
    CertificateError, DigitallySignedStruct, Error, RootCertStore, SignatureScheme,
    client::{WebPkiServerVerifier, danger::ServerCertVerifier},
    pki_types::{CertificateDer, UnixTime},
};
use tracing::debug;
use x509_cert::Certificate;
use x509_parser::{
    asn1_rs::{BitString as X509BitString, FromDer as X509FromDer},
    verify::verify_signature as verify_x509_signature,
    x509::AlgorithmIdentifier as X509AlgorithmIdentifier,
};

use super::wire::{
    BasicOcspResponse, OcspStatus, build_cert_id, decode_unvalidated_ocsp_response_der, der_error,
    matches_cert_id, parse_x509_certificate_der, responder_id_matches_certificate,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissingStapledOcspPolicy {
    AllowMissing,
    RequirePresent,
}

#[derive(Debug)]
struct OcspServerCertVerifier {
    inner: Arc<WebPkiServerVerifier>,
    missing_policy: MissingStapledOcspPolicy,
}

pub fn ocsp_server_cert_verifier(root_store: Arc<RootCertStore>) -> Arc<dyn ServerCertVerifier> {
    ocsp_server_cert_verifier_with_missing_policy(
        root_store,
        MissingStapledOcspPolicy::RequirePresent,
    )
}

pub fn ocsp_server_cert_verifier_with_missing_policy(
    root_store: Arc<RootCertStore>,
    missing_policy: MissingStapledOcspPolicy,
) -> Arc<dyn ServerCertVerifier> {
    Arc::new(OcspServerCertVerifier {
        inner: webpki_verifier(root_store),
        missing_policy,
    })
}

impl ServerCertVerifier for OcspServerCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &rustls::pki_types::ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, Error> {
        let verified = self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        )?;

        if ocsp_response.is_empty() {
            match self.missing_policy {
                MissingStapledOcspPolicy::AllowMissing => {
                    debug!(
                        server_name = ?server_name,
                        "server certificate handshake did not include a stapled OCSP response"
                    );
                }
                MissingStapledOcspPolicy::RequirePresent => {
                    return Err(Error::InvalidCertificate(
                        CertificateError::InvalidOcspResponse,
                    ));
                }
            }

            return Ok(verified);
        }

        let Some(issuer) = intermediates.first() else {
            return Err(Error::InvalidCertificate(
                CertificateError::InvalidOcspResponse,
            ));
        };

        match verify_stapled_ocsp_response(end_entity, issuer, ocsp_response, now)? {
            OcspStatus::Good => {
                debug!(
                    server_name = ?server_name,
                    response_len = ocsp_response.len(),
                    "validated stapled OCSP response for server certificate"
                );
            }
            OcspStatus::Revoked => {
                return Err(Error::InvalidCertificate(CertificateError::Revoked));
            }
            OcspStatus::Unknown => {
                return Err(Error::InvalidCertificate(
                    CertificateError::UnknownRevocationStatus,
                ));
            }
        }

        Ok(verified)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

fn webpki_verifier(root_store: Arc<RootCertStore>) -> Arc<WebPkiServerVerifier> {
    WebPkiServerVerifier::builder(root_store)
        .build()
        .expect("webpki verifier must build from gateway root store")
}

fn verify_stapled_ocsp_response(
    end_entity: &CertificateDer<'_>,
    issuer: &CertificateDer<'_>,
    response_der: &[u8],
    now: UnixTime,
) -> Result<OcspStatus, Error> {
    let end_entity_cert = Certificate::from_der(end_entity.as_ref())
        .map_err(|_| Error::InvalidCertificate(CertificateError::InvalidOcspResponse))?;
    let issuer_cert = Certificate::from_der(issuer.as_ref())
        .map_err(|_| Error::InvalidCertificate(CertificateError::InvalidOcspResponse))?;
    let parsed = decode_unvalidated_ocsp_response_der(response_der, now)?;
    let single = parsed
        .basic
        .tbs_response_data
        .responses
        .first()
        .expect("single response checked during OCSP decode");
    let expected_cert_id = build_cert_id(&end_entity_cert, &issuer_cert)?;

    if !matches_cert_id(&single.cert_id, &expected_cert_id) {
        return Err(Error::InvalidCertificate(
            CertificateError::InvalidOcspResponse,
        ));
    }

    verify_ocsp_signer(&parsed.basic, &issuer_cert, issuer.as_ref(), now)?;

    Ok(parsed.status)
}

fn verify_ocsp_signer(
    basic: &BasicOcspResponse,
    issuer_cert: &Certificate,
    issuer_der: &[u8],
    now: UnixTime,
) -> Result<(), Error> {
    let mut candidate_ders = Vec::new();

    if responder_id_matches_certificate(&basic.tbs_response_data.responder_id, issuer_cert)? {
        candidate_ders.push(issuer_der.to_vec());
    }

    if let Some(certs) = &basic.certs {
        for cert in certs {
            if !responder_id_matches_certificate(&basic.tbs_response_data.responder_id, cert)? {
                continue;
            }

            let cert_der = cert.to_der().map_err(der_error)?;
            validate_delegated_ocsp_signer(&cert_der, issuer_der, now)?;
            candidate_ders.push(cert_der);
        }
    }

    if candidate_ders.is_empty() {
        return Err(Error::InvalidCertificate(
            CertificateError::InvalidOcspResponse,
        ));
    }

    for candidate_der in candidate_ders {
        if verify_basic_ocsp_signature(basic, &candidate_der).is_ok() {
            return Ok(());
        }
    }

    Err(Error::InvalidCertificate(
        CertificateError::InvalidOcspResponse,
    ))
}

fn validate_delegated_ocsp_signer(
    responder_der: &[u8],
    issuer_der: &[u8],
    now: UnixTime,
) -> Result<(), Error> {
    let responder =
        parse_x509_certificate_der(responder_der, "delegated OCSP responder certificate")?;
    let issuer = parse_x509_certificate_der(issuer_der, "issuer certificate")?;
    let now =
        x509_parser::time::ASN1Time::from_timestamp(now.as_secs() as i64).map_err(|error| {
            Error::General(format!("failed to convert OCSP verification time: {error}"))
        })?;

    if !responder.validity().is_valid_at(now) {
        return Err(Error::General(
            "delegated OCSP responder certificate is not valid at the handshake time".to_owned(),
        ));
    }

    responder
        .verify_signature(Some(issuer.public_key()))
        .map_err(|error| {
            Error::General(format!(
                "delegated OCSP responder certificate was not signed by the issuer: {error}"
            ))
        })?;

    let eku = responder
        .extended_key_usage()
        .map_err(|error| {
            Error::General(format!(
                "failed to read delegated OCSP responder certificate EKU: {error}"
            ))
        })?
        .ok_or_else(|| {
            Error::General(
                "delegated OCSP responder certificate is missing the OCSPSigning EKU".to_owned(),
            )
        })?;

    if !eku.value.ocsp_signing {
        return Err(Error::General(
            "delegated OCSP responder certificate is missing the OCSPSigning EKU".to_owned(),
        ));
    }

    if let Some(key_usage) = responder.key_usage().map_err(|error| {
        Error::General(format!(
            "failed to read delegated OCSP responder certificate key usage: {error}"
        ))
    })? && !key_usage.value.digital_signature()
    {
        return Err(Error::General(
            "delegated OCSP responder certificate key usage does not allow digital signatures"
                .to_owned(),
        ));
    }

    Ok(())
}

fn verify_basic_ocsp_signature(basic: &BasicOcspResponse, signer_der: &[u8]) -> Result<(), Error> {
    let signer = parse_x509_certificate_der(signer_der, "OCSP signer certificate")?;
    let signature_algorithm_der = basic.signature_algorithm.to_der().map_err(der_error)?;
    let (_, signature_algorithm) = X509AlgorithmIdentifier::from_der(&signature_algorithm_der)
        .map_err(|error| {
            Error::General(format!(
                "failed to parse OCSP response signature algorithm: {error}"
            ))
        })?;
    let signature_der = basic.signature.to_der().map_err(der_error)?;
    let (_, signature_value) = X509BitString::from_der(&signature_der).map_err(|error| {
        Error::General(format!(
            "failed to parse OCSP response signature value: {error}"
        ))
    })?;
    let tbs_der = basic.tbs_response_data.to_der().map_err(der_error)?;

    verify_x509_signature(
        signer.public_key(),
        &signature_algorithm,
        &signature_value,
        &tbs_der,
    )
    .map_err(|error| Error::General(format!("failed to verify OCSP response signature: {error}")))
}
