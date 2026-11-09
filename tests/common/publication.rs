use super::*;

pub(crate) fn registry_signer() -> dhttp::LocalAuthority {
    let mut params =
        rcgen::CertificateParams::new(vec![crate::package::store::STORE_NAME.into()]).unwrap();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, crate::package::store::STORE_NAME);
    params.is_ca = rcgen::IsCa::ExplicitNoCa;
    params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    params.key_identifier_method = rcgen::KeyIdMethod::PreSpecified(
        format!("1:0:{}", crate::package::store::STORE_OWNER).into_bytes(),
    );
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).unwrap();
    let cert = params.signed_by(&key, &signing_issuers().0).unwrap();
    dhttp::LocalAuthority::new(
        &qtls::default_provider(),
        Arc::from(crate::package::store::STORE_NAME),
        vec![cert.der().clone(), signing_root()],
        qtls::PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
        vec![1],
    )
    .unwrap()
}
pub(crate) fn sign_publication(proof: &mut Value) {
    let signer = registry_signer();
    let payload=serde_json::to_vec(&json!({"format":"pishoo-publication/1","registry":crate::package::store::STORE_NAME,"registry_owner_hash":crate::package::store::STORE_OWNER,
        "author":proof["author"],"name":proof["name"],"version":proof["version"],"package_sha256":proof["package_sha256"],"size":proof["size"],"author_owner_hash":proof["author_owner_hash"],
        "leaf_fingerprint_sha256":proof["signature"]["leaf_fingerprint_sha256"],"issuer_id":proof["signature"]["issuer_id"],"verified_at":proof["signature"]["verified_at"]})).unwrap();
    let mut input = b"pishoo-publication-receipt-v1\0".to_vec();
    input.extend_from_slice(&payload);
    proof["publication_receipt"] = json!({"payload":STANDARD.encode(payload),"signature":STANDARD.encode(signer.sign(qtls::SignatureScheme::ECDSA_NISTP384_SHA384,&input).unwrap()),"certificates":signer.certificates().iter().map(|c|STANDARD.encode(c.as_ref())).collect::<Vec<_>>()});
}
pub(crate) fn revocation_list(bytes: &[u8], number: u64, revoked: bool, expired: bool) -> Vec<u8> {
    use rcgen::{CertificateRevocationListParams, RevokedCertParams};
    let files = zip_files(bytes);
    let chain = crate::package::signing::certificates(&files).unwrap();
    let (_, cert) = x509_parser::parse_x509_certificate(chain[0].as_ref()).unwrap();
    let mut this_update = rcgen::date_time_ymd(2026, 1, 1);
    let next_update = if expired {
        rcgen::date_time_ymd(2026, 1, 2)
    } else {
        rcgen::date_time_ymd(2028, 1, 1)
    };
    if expired {
        this_update = rcgen::date_time_ymd(2025, 1, 1);
    }
    CertificateRevocationListParams {
        this_update,
        next_update,
        crl_number: number.into(),
        issuing_distribution_point: None,
        revoked_certs: if revoked {
            vec![RevokedCertParams {
                serial_number: cert.raw_serial().to_vec().into(),
                revocation_time: this_update,
                reason_code: None,
                invalidity_date: None,
            }]
        } else {
            vec![]
        },
        key_identifier_method: rcgen::KeyIdMethod::Sha256,
    }
    .signed_by(&signing_issuers().1)
    .unwrap()
    .der()
    .to_vec()
}

pub(crate) fn release_proof(bytes: &[u8]) -> Value {
    release_proof_at(bytes, chrono::Utc::now().timestamp())
}
pub(crate) fn release_proof_at(bytes: &[u8], now: i64) -> Value {
    let package = crate::package::format::read_package_at(bytes, now).unwrap();
    let doc = crate::package::package_metadata(&package).unwrap();
    let facts =
        crate::package::signing::facts(&package, doc["publisher"].as_str().unwrap(), now).unwrap();
    let envelope: Value = serde_json::from_slice(&package["signature.json"]).unwrap();
    let mut proof = doc.clone();
    proof["author"] = doc["publisher"].clone();
    proof.as_object_mut().unwrap().remove("publisher");
    proof["dependencies"] = doc["dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|dep| {
            let mut item = json!({"name":dep["name"],"version":dep["version"]});
            if let Some(author) = dep.get("publisher") {
                item["author"] = author.clone();
            }
            item
        })
        .collect::<Vec<_>>()
        .into();
    proof["package_sha256"] = format!("{:x}", Sha256::digest(bytes)).into();
    proof["size"] = bytes.len().into();
    proof["author_owner_hash"] = facts["author_owner_hash"].clone();
    proof["signer_certificates"] = envelope["certificates"].clone();
    proof["signature"] = facts.clone();
    proof["signature"]["purpose"] = "code_signing".into();
    proof["signature"]["verified_at"] = now.into();
    sign_publication(&mut proof);
    proof
}
