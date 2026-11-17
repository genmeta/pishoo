//! Fetch, validate and cache identity OCSP proofs for loading and renewal.
use dhttp_home::identity::IdentityProfile;

use super::files::inherit_identity_owner;
use crate::{Error, Result};

pub(crate) fn cache_ocsp(profile: &IdentityProfile, response: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut cache = tempfile::NamedTempFile::new_in(profile.ssl_dir())?;
    inherit_identity_owner(cache.as_file(), profile)?;
    cache.write_all(response)?;
    cache.as_file().sync_all()?;
    cache
        .persist(profile.ocsp_path())
        .map_err(|error| error.error)?;
    #[cfg(unix)]
    std::fs::File::open(profile.ssl_dir())?.sync_all()?;
    Ok(())
}

pub(crate) async fn fetch_ocsp(certificates: &[dhttp::CertificateDer<'_>]) -> Result<Vec<u8>> {
    use der::{Decode, Encode};
    use sha1::{Digest, Sha1};
    use x509_parser::prelude::FromDer;
    let error = |error: String| Error::InvalidIdentity(format!("OCSP: {error}"));
    let leaf = certificates
        .first()
        .ok_or_else(|| error("missing certificate".into()))?;
    let issuer = certificates
        .get(1)
        .ok_or_else(|| error("missing issuer certificate".into()))?;
    let (_, parsed) = x509_parser::certificate::X509Certificate::from_der(leaf.as_ref())
        .map_err(|e| error(e.to_string()))?;
    if !parsed.validity().is_valid() {
        return Err(error("certificate is outside its validity period".into()));
    }
    let url = parsed
        .extensions()
        .iter()
        .find_map(|extension| {
            let x509_parser::extensions::ParsedExtension::AuthorityInfoAccess(aia) =
                extension.parsed_extension()
            else {
                return None;
            };
            aia.accessdescs.iter().find_map(|access| {
                if access.access_method.to_id_string() != "1.3.6.1.5.5.7.48.1" {
                    return None;
                }
                match &access.access_location {
                    x509_parser::extensions::GeneralName::URI(uri) => Some((*uri).to_owned()),
                    _ => None,
                }
            })
        })
        .ok_or_else(|| error("certificate has no OCSP responder URI".into()))?;
    let leaf = x509_cert::Certificate::from_der(leaf.as_ref()).map_err(|e| error(e.to_string()))?;
    let issuer =
        x509_cert::Certificate::from_der(issuer.as_ref()).map_err(|e| error(e.to_string()))?;
    let request = x509_ocsp::OcspRequest {
        tbs_request: x509_ocsp::TbsRequest {
            request_list: vec![x509_ocsp::Request {
                req_cert: x509_ocsp::CertId {
                    hash_algorithm: x509_cert::spki::AlgorithmIdentifierOwned {
                        oid: "1.3.14.3.2.26"
                            .parse::<der::asn1::ObjectIdentifier>()
                            .map_err(|e| error(e.to_string()))?,
                        parameters: Some(der::asn1::Any::null()),
                    },
                    issuer_name_hash: der::asn1::OctetString::new(
                        Sha1::digest(
                            issuer
                                .tbs_certificate
                                .subject
                                .to_der()
                                .map_err(|e| error(e.to_string()))?,
                        )
                        .to_vec(),
                    )
                    .map_err(|e| error(e.to_string()))?,
                    issuer_key_hash: der::asn1::OctetString::new(
                        Sha1::digest(
                            issuer
                                .tbs_certificate
                                .subject_public_key_info
                                .subject_public_key
                                .raw_bytes(),
                        )
                        .to_vec(),
                    )
                    .map_err(|e| error(e.to_string()))?,
                    serial_number: leaf.tbs_certificate.serial_number,
                },
                single_request_extensions: None,
            }],
            ..Default::default()
        },
        optional_signature: None,
    }
    .to_der()
    .map_err(|e| error(e.to_string()))?;
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .https_only(false)
            .build()
            .map_err(|e| error(e.to_string()))?;
        let uri = reqwest::Url::parse(&url).map_err(|e| error(e.to_string()))?;
        if !matches!(uri.scheme(), "http" | "https") {
            return Err(error("unsupported responder scheme".into()));
        }
        let mut response = client
            .post(uri)
            .header("Content-Type", "application/ocsp-request")
            .header("Accept", "application/ocsp-response")
            .body(request)
            .send()
            .await
            .map_err(|e| error(e.to_string()))?;
        if !response.status().is_success() {
            return Err(error(format!("responder returned {}", response.status())));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| error(e.to_string()))? {
            if body.len().saturating_add(chunk.len()) > 64 * 1024 {
                return Err(error("response exceeds 64KiB".into()));
            }
            body.extend_from_slice(&chunk);
        }
        qtls::validate_ocsp(&body, certificates, qtls::UnixTime::now())
            .map_err(|e| error(e.to_string()))?;
        Ok(body)
    })
    .await
    .map_err(|_| error("responder timed out after 15 seconds".into()))?
}

pub(super) async fn renew_ocsp(
    profile: &IdentityProfile,
    endpoint: &dhttp::Endpoint,
) -> Result<dhttp::Endpoint> {
    use std::time::Duration;

    // Renew the loaded certificate's proof; certificate/key rotation still requires restart.
    let authority = endpoint.local_authority()?;
    let cached = profile.load_ocsp().await.ok();
    let now = qtls::UnixTime::now();
    let mut response = authority.ocsp();
    let mut expiry = ocsp_expiry(response, authority.certificates(), now).ok();
    // Keep the longest still-valid proof, including a cache updated outside this process.
    // Prefer existing bytes on ties so an equal lifetime does not replace the cache.
    if let Some(existing) = cached.as_deref()
        && let Ok(existing_expiry) = ocsp_expiry(existing, authority.certificates(), now)
        && expiry.is_none_or(|expiry| existing_expiry >= expiry)
    {
        response = existing;
        expiry = Some(existing_expiry);
    }
    // Leave a full 72-hour renewal window after the next periodic check.
    let fetched = if expiry.is_some_and(|expiry| {
        expiry.saturating_sub(Duration::from_secs(now.as_secs()))
            > Duration::from_secs(2 * 72 * 60 * 60)
    }) {
        None
    } else {
        Some(fetch_ocsp(authority.certificates()).await?)
    };
    if let Some(fetched) = fetched.as_deref() {
        let fetched_expiry = ocsp_expiry(fetched, authority.certificates(), qtls::UnixTime::now())?;
        if expiry.is_none_or(|expiry| fetched_expiry > expiry) {
            response = fetched;
        }
    }
    if cached.as_deref() != Some(response) {
        cache_ocsp(profile, response)?;
    }
    if authority.ocsp() == response {
        return Ok(endpoint.clone());
    }
    // Root services can host profiles from other accounts or the global home.
    endpoint
        .reload_from(profile.path())
        .await
        .map_err(Into::into)
}

fn ocsp_expiry(
    response: &[u8],
    certificates: &[dhttp::CertificateDer<'_>],
    now: qtls::UnixTime,
) -> Result<std::time::Duration> {
    use der::Decode;

    let error = |error: String| Error::InvalidIdentity(format!("OCSP: {error}"));
    // Only a proof valid for this certificate may prevent a renewal from replacing it.
    qtls::validate_ocsp(response, certificates, now).map_err(|e| error(e.to_string()))?;
    let leaf = x509_cert::Certificate::from_der(certificates[0].as_ref())
        .map_err(|e| error(e.to_string()))?;
    let response = x509_ocsp::OcspResponse::from_der(response)
        .map_err(|e| error(e.to_string()))?
        .response_bytes
        .ok_or_else(|| error("missing response bytes".into()))?;
    let response = x509_ocsp::BasicOcspResponse::from_der(response.response.as_bytes())
        .map_err(|e| error(e.to_string()))?;
    response
        .tbs_response_data
        .responses
        .iter()
        .find(|single| single.cert_id.serial_number == leaf.tbs_certificate.serial_number)
        .and_then(|single| single.next_update)
        .map(|next_update| next_update.0.to_unix_duration())
        .ok_or_else(|| error("missing certificate nextUpdate".into()))
}
