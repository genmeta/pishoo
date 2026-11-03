//! Offline developer trust, portable publication receipts and optional signed CRLs.
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{Error, Result};

pub(super) fn policy(code: &'static str, status: http::StatusCode) -> Error {
    Error::PackagePolicy { code, status }
}
pub(super) fn certificates(
    files: &super::PackageFiles,
) -> Result<Vec<qtls::CertificateDer<'static>>> {
    let bad = || {
        policy(
            "developer_certificate_required",
            http::StatusCode::UNPROCESSABLE_ENTITY,
        )
    };
    let envelope: Value =
        serde_json::from_slice(files.get("signature.json").ok_or_else(bad)?).map_err(|_| bad())?;
    envelope["certificates"]
        .as_array()
        .filter(|chain| (2..=3).contains(&chain.len()))
        .ok_or_else(bad)?
        .iter()
        .map(|cert| {
            STANDARD
                .decode(cert.as_str().ok_or_else(bad)?)
                .map(qtls::CertificateDer::from)
                .map_err(|_| bad())
        })
        .collect()
}

pub(super) fn verify_chain(
    chain: &[qtls::CertificateDer<'_>],
    domain: &str,
    at: i64,
) -> Result<()> {
    #[cfg(not(test))]
    {
        verify_chain_with_root(chain, domain, at, dhttp::trust::DHTTP_ROOT_CA_DER)
    }
    #[cfg(test)]
    {
        test_support::verify_fixture_chain(chain, domain, at)
    }
}
fn verify_chain_with_root(
    chain: &[qtls::CertificateDer<'_>],
    domain: &str,
    at: i64,
    root_der: &[u8],
) -> Result<()> {
    let bad = || {
        policy(
            "signing_certificate_untrusted",
            http::StatusCode::UNPROCESSABLE_ENTITY,
        )
    };
    if !(2..=3).contains(&chain.len()) || at < 0 {
        return Err(bad());
    }
    let mut parsed = Vec::new();
    for der in chain {
        let (remaining, cert) =
            x509_parser::parse_x509_certificate(der.as_ref()).map_err(|_| bad())?;
        if !remaining.is_empty() {
            return Err(bad());
        }
        parsed.push(cert);
    }
    for (index, cert) in parsed.iter().take(2).enumerate() {
        // Both the leaf and the dedicated code-signing issuer use P-384.
        if cert.public_key().algorithm.algorithm.to_id_string() != "1.2.840.10045.2.1"
            || cert
                .public_key()
                .algorithm
                .parameters
                .as_ref()
                .and_then(|p| p.as_oid().ok())
                .is_none_or(|oid| oid.to_id_string() != "1.3.132.0.34")
            || cert.signature_algorithm.algorithm.to_id_string() != "1.2.840.10045.4.3.3"
        {
            return Err(bad());
        }
        let bc = cert
            .basic_constraints()
            .map_err(|_| bad())?
            .ok_or_else(bad)?;
        let ku = cert.key_usage().map_err(|_| bad())?.ok_or_else(bad)?;
        let eku = cert
            .extended_key_usage()
            .map_err(|_| bad())?
            .ok_or_else(bad)?;
        if !bc.critical
            || !ku.critical
            || !eku.value.code_signing
            || eku.value.any
            || (eku.value.server_auth != eku.value.client_auth)
            || eku.value.email_protection
            || eku.value.time_stamping
            || eku.value.ocsp_signing
            || !eku.value.other.is_empty()
            || (index == 0
                && (bc.value.ca || bc.value.path_len_constraint.is_some() || ku.value.flags != 1))
            || (index == 1
                && (!bc.value.ca
                    || bc.value.path_len_constraint != Some(0)
                    || ku.value.flags & 33 != 33
                    || ku.value.flags & !97 != 0))
        {
            return Err(policy(
                "developer_certificate_required",
                http::StatusCode::UNPROCESSABLE_ENTITY,
            ));
        }
    }
    let names = parsed[0]
        .subject_alternative_name()
        .map_err(|_| bad())?
        .ok_or_else(bad)?;
    if names.value.general_names.len() != 1
        || !matches!(&names.value.general_names[0], x509_parser::extensions::GeneralName::DNSName(name) if *name == domain)
        || dhttp_home::normalize_name(domain).as_deref() != Some(domain)
    {
        return Err(policy(
            "signing_identity_mismatch",
            http::StatusCode::FORBIDDEN,
        ));
    }
    let common_names = parsed[0].subject().iter_common_name().collect::<Vec<_>>();
    if common_names.len() != 1 || common_names[0].as_str().ok() != Some(domain) {
        return Err(policy(
            "signing_identity_mismatch",
            http::StatusCode::FORBIDDEN,
        ));
    }
    let ski =
        dhttp_home::certificate::extract_dhttp_subject_key_identifier(chain).map_err(|_| bad())?;
    if ski.chain().usage() != dhttp_home::certificate::CertificateUsage::ClientAndServer {
        return Err(bad());
    }
    let root = qtls::CertificateDer::from(root_der);
    if chain.len() == 3 && chain[2].as_ref() != root.as_ref() {
        return Err(bad());
    }
    let anchor = webpki::anchor_from_trusted_cert(&root).map_err(|_| bad())?;
    let leaf = webpki::EndEntityCert::try_from(&chain[0]).map_err(|_| bad())?;
    leaf.verify_for_usage(
        webpki::ALL_VERIFICATION_ALGS,
        &[anchor],
        &chain[1..2],
        qtls::UnixTime::since_unix_epoch(Duration::from_secs(at as u64)),
        webpki::KeyUsage::required(&[0x2b, 6, 1, 5, 5, 7, 3, 3]),
        None,
        None,
    )
    .map_err(|_| bad())?;
    Ok(())
}

pub(super) fn facts(files: &super::PackageFiles, domain: &str, at: i64) -> Result<Value> {
    let chain = certificates(files)?;
    verify_chain(&chain, domain, at)?;
    let (_, leaf) = x509_parser::parse_x509_certificate(chain[0].as_ref()).unwrap();
    let ski = dhttp_home::certificate::extract_dhttp_subject_key_identifier(&chain)
        .map_err(|_| policy("signing_identity_mismatch", http::StatusCode::FORBIDDEN))?;
    Ok(
        serde_json::json!({"leaf_fingerprint_sha256":format!("{:x}",Sha256::digest(chain[0].as_ref())),
        "issuer_id":format!("{:x}",Sha256::digest(chain[1].as_ref())),
        "serial_number":leaf.raw_serial().iter().map(|b|format!("{b:02X}")).collect::<String>(),
        "ski":ski.to_string(),"sequence":ski.chain().sequence().get(),"leaf_der":STANDARD.encode(chain[0].as_ref()),"author_owner_hash":ski.owner_hash().as_str(),
        "valid_not_before":leaf.validity().not_before.timestamp(),"valid_not_after":leaf.validity().not_after.timestamp()}),
    )
}
/// Verify portable Registry evidence before its historical time is used.
pub(super) fn verify_receipt(metadata: &Value, at: i64) -> Result<()> {
    if at <= 0
        || at > chrono::Utc::now().timestamp()
        || metadata["signature"]["purpose"] != "code_signing"
    {
        return Err(policy(
            "package_publication_invalid",
            http::StatusCode::UNPROCESSABLE_ENTITY,
        ));
    }
    #[cfg(not(test))]
    let root = dhttp::trust::DHTTP_ROOT_CA_DER.to_vec();
    #[cfg(test)]
    let root = crate::package::tests::packages::signing_root()
        .as_ref()
        .to_vec();
    verify_receipt_with_root(metadata, at, &root)
}

fn verify_receipt_with_root(metadata: &Value, at: i64, root: &[u8]) -> Result<()> {
    let bad = || {
        policy(
            "package_publication_invalid",
            http::StatusCode::UNPROCESSABLE_ENTITY,
        )
    };
    let receipt = &metadata["publication_receipt"];
    let payload = STANDARD
        .decode(receipt["payload"].as_str().ok_or_else(bad)?)
        .map_err(|_| bad())?;
    if payload.len() > 16 * 1024 {
        return Err(bad());
    }
    let value: Value = serde_json::from_slice(&payload).map_err(|_| bad())?;
    let expected = serde_json::json!({
        "format":"pishoo-publication/1", "registry":super::store::STORE_NAME,
        "registry_owner_hash":super::store::STORE_OWNER,
        "author":metadata["author"], "name":metadata["name"], "version":metadata["version"],
        "package_sha256":metadata["package_sha256"], "size":metadata["size"],
        "author_owner_hash":metadata["author_owner_hash"],
        "leaf_fingerprint_sha256":metadata["signature"]["leaf_fingerprint_sha256"],
        "issuer_id":metadata["signature"]["issuer_id"], "verified_at":at
    });
    if value != expected {
        return Err(bad());
    }
    let chain = receipt["certificates"]
        .as_array()
        .filter(|c| (2..=3).contains(&c.len()))
        .ok_or_else(bad)?
        .iter()
        .map(|c| {
            STANDARD
                .decode(c.as_str().ok_or_else(bad)?)
                .map(qtls::CertificateDer::from)
                .map_err(|_| bad())
        })
        .collect::<Result<Vec<_>>>()?;
    if chain.len() == 3 && chain[2].as_ref() != root {
        return Err(bad());
    }
    let root = qtls::CertificateDer::from(root);
    let anchor = webpki::anchor_from_trusted_cert(&root).map_err(|_| bad())?;
    let leaf = webpki::EndEntityCert::try_from(&chain[0]).map_err(|_| bad())?;
    leaf.verify_for_usage(
        webpki::ALL_VERIFICATION_ALGS,
        &[anchor],
        &chain[1..2],
        qtls::UnixTime::since_unix_epoch(Duration::from_secs(at.try_into().map_err(|_| bad())?)),
        webpki::KeyUsage::server_auth(),
        None,
        None,
    )
    .map_err(|_| bad())?;
    leaf.verify_is_valid_for_subject_name(
        &qtls::ServerName::try_from(super::store::STORE_NAME).map_err(|_| bad())?,
    )
    .map_err(|_| bad())?;
    let ski =
        dhttp_home::certificate::extract_dhttp_subject_key_identifier(&chain).map_err(|_| bad())?;
    if ski.owner_hash().as_str() != super::store::STORE_OWNER {
        return Err(bad());
    }
    let (rest, cert) = x509_parser::parse_x509_certificate(chain[0].as_ref()).map_err(|_| bad())?;
    if !rest.is_empty() {
        return Err(bad());
    }
    let signature = STANDARD
        .decode(receipt["signature"].as_str().ok_or_else(bad)?)
        .map_err(|_| bad())?;
    let mut input = b"pishoo-publication-receipt-v1\0".to_vec();
    input.extend_from_slice(&payload);
    if !dhttp_home::certificate::verify_signature(cert.public_key().raw, &input, &signature)
        .map_err(|_| bad())?
    {
        return Err(bad());
    }
    Ok(())
}

/// Optional issuer-signed CRLs are imported independently of package operations.
/// No list means no claim about current revocation, never an online lookup.
pub(super) fn check_revocations(
    files: &super::PackageFiles,
    directory: &std::path::Path,
    supplied: &Value,
    now: i64,
) -> Result<()> {
    let chain = certificates(files)?;
    let issuer_id = format!("{:x}", Sha256::digest(chain[1].as_ref()));
    let path = directory.join(format!("{issuer_id}.crl"));
    let cached = match std::fs::read(&path) {
        Ok(bytes) if bytes.len() <= 4 * 1024 * 1024 => Some(bytes),
        Ok(_) => {
            return Err(policy(
                "signing_revocation_list_invalid",
                http::StatusCode::UNPROCESSABLE_ENTITY,
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let incoming = supplied
        .as_str()
        .map(|s| {
            STANDARD.decode(s).map_err(|_| {
                policy(
                    "signing_revocation_list_invalid",
                    http::StatusCode::UNPROCESSABLE_ENTITY,
                )
            })
        })
        .transpose()?;
    // Authenticate both snapshots before comparing their monotonic CRL numbers.
    let old = cached
        .as_deref()
        .map(|b| validate_crl(b, &chain, now, false))
        .transpose()?;
    let new = incoming
        .as_deref()
        .map(|b| validate_crl(b, &chain, now, false))
        .transpose()?;
    let selected = match (&old, &new) {
        (Some(old), Some(new)) if old >= new => cached.as_deref(),
        (_, Some(_)) => incoming.as_deref(),
        _ => cached.as_deref(),
    };
    if let Some(bytes) = selected {
        if incoming.as_deref() == Some(bytes) && new > old {
            std::fs::create_dir_all(directory)?;
            let mut file = tempfile::NamedTempFile::new_in(directory)?;
            use std::io::Write;
            file.write_all(bytes)?;
            file.as_file().sync_all()?;
            file.persist(path).map_err(|e| e.error)?;
            std::fs::File::open(directory)?.sync_all()?;
        }
        validate_crl(bytes, &chain, now, true)?;
    }
    Ok(())
}

fn validate_crl(
    bytes: &[u8],
    chain: &[qtls::CertificateDer<'_>],
    now: i64,
    enforce: bool,
) -> Result<Vec<u8>> {
    use x509_parser::prelude::FromDer;
    let bad = || {
        policy(
            "signing_revocation_list_invalid",
            http::StatusCode::UNPROCESSABLE_ENTITY,
        )
    };
    if bytes.len() > 4 * 1024 * 1024 {
        return Err(bad());
    }
    let (rest, crl) = x509_parser::revocation_list::CertificateRevocationList::from_der(bytes)
        .map_err(|_| bad())?;
    let (_, issuer) = x509_parser::parse_x509_certificate(chain[1].as_ref()).map_err(|_| bad())?;
    let (_, leaf) = x509_parser::parse_x509_certificate(chain[0].as_ref()).map_err(|_| bad())?;
    if !rest.is_empty()
        || crl.issuer() != issuer.subject()
        || issuer
            .key_usage()
            .map_err(|_| bad())?
            .is_none_or(|ku| !ku.value.crl_sign())
        || crl.signature_algorithm.algorithm.to_id_string() != "1.2.840.10045.4.3.3"
        || crl.tbs_cert_list.signature != crl.signature_algorithm
        || crl.tbs_cert_list.extensions_map().is_err()
        || crl.extensions().iter().any(|e| e.critical)
        || crl
            .iter_revoked_certificates()
            .any(|e| e.extensions().iter().any(|x| x.critical))
        || !dhttp_home::certificate::verify_signature(
            issuer.public_key().raw,
            crl.tbs_cert_list.as_ref(),
            crl.signature_value.data.as_ref(),
        )
        .map_err(|_| bad())?
    {
        return Err(bad());
    }
    let number = crl.crl_number().ok_or_else(bad)?.to_bytes_be();
    // Length first makes byte-wise comparison a numeric comparison.
    if number.len() > 20 {
        return Err(bad());
    }
    let mut ordered = vec![number.len() as u8];
    ordered.extend(number);
    if enforce {
        // Known revocations remain denied even when the list has expired.
        if crl
            .iter_revoked_certificates()
            .any(|e| e.raw_serial() == leaf.raw_serial())
        {
            return Err(policy(
                "signing_certificate_revoked",
                http::StatusCode::FORBIDDEN,
            ));
        }
        let start = crl.last_update().timestamp();
        if start > now
            || crl
                .next_update()
                .is_none_or(|t| t.timestamp() <= now || t.timestamp() <= start)
        {
            return Err(policy(
                "signing_revocation_list_stale",
                http::StatusCode::SERVICE_UNAVAILABLE,
            ));
        }
    }
    Ok(ordered)
}

#[cfg(test)]
#[path = "../../tests/unit/package/signing/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "../../tests/common/package_signing.rs"]
mod test_support;
