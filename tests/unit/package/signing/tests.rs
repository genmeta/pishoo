use super::*;
use crate::package::tests::packages::{frontend_document, package_fixture, publisher, with_proof};

#[test]
fn signer_names_are_unrestricted_but_must_match_the_signed_author() {
    for name in [
        "alex.chen.dhttp.net",
        "dev.wang.dhttp.net",
        "dev.dev.wang.dhttp.net",
        "std.dhttp.net",
    ] {
        let signer = publisher(name, 'a');
        let mut document = frontend_document("1.0.0");
        document["x-pishoo"]["publisher"] = name.into();
        let files = || std::collections::BTreeMap::from([("index.html".into(), b"page".to_vec())]);
        let bytes = package_fixture(&document, &[], files(), &signer);
        crate::package::read_package(&bytes).unwrap();
        document["x-pishoo"]["publisher"] = "other.wang.dhttp.net".into();
        let mismatched = package_fixture(&document, &[], files(), &signer);
        assert!(crate::package::read_package(&mismatched).is_err(), "{name}");
    }
}

fn fixture(
    name: &str,
    version: &str,
    owner: char,
    dependencies: &[&str],
) -> (String, Vec<u8>, Value) {
    let mut doc = frontend_document(version);
    doc["x-pishoo"]["name"] = name.into();
    doc["x-pishoo"]["dependencies"] = dependencies
        .iter()
        .map(
            |name| serde_json::json!({"publisher":"alice.dhttp.net","name":name,"version":version}),
        )
        .collect::<Vec<_>>()
        .into();
    let bytes = package_fixture(
        &doc,
        &[],
        std::collections::BTreeMap::from([("index.html".into(), b"page".to_vec())]),
        &publisher("alice.dhttp.net", owner),
    );
    with_proof((format!("alice.dhttp.net/{name}"), bytes))
}
fn pem(bytes: &[u8]) -> qtls::CertificateDer<'static> {
    x509_parser::pem::parse_x509_pem(bytes)
        .unwrap()
        .1
        .contents
        .into()
}
fn vector(name: &str) -> Vec<qtls::CertificateDer<'static>> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/code-signing");
    let issuer = if name == "tls" {
        "tls-issuer"
    } else {
        "code-issuer"
    };
    vec![
        pem(&std::fs::read(root.join(format!("{name}.cert.pem"))).unwrap()),
        pem(&std::fs::read(root.join(format!("{issuer}.cert.pem"))).unwrap()),
    ]
}
#[test]
fn shared_vectors_require_code_signing_and_trusted_historical_time() {
    let root = pem(include_bytes!(
        "../../../fixtures/code-signing/root.cert.pem"
    ));
    let now = chrono::Utc::now().timestamp();
    verify_chain_with_root(
        &vector("developer"),
        "developer.dhttp.net",
        now,
        root.as_ref(),
    )
    .unwrap();
    for name in ["tls", "any-eku", "missing-eku", "wrong-issuer"] {
        assert!(
            verify_chain_with_root(&vector(name), "developer.dhttp.net", now, root.as_ref())
                .is_err(),
            "{name}"
        );
    }
    assert!(
        verify_chain_with_root(
            &vector("expired"),
            "developer.dhttp.net",
            now,
            root.as_ref()
        )
        .is_err()
    );
    verify_chain_with_root(
        &vector("expired"),
        "developer.dhttp.net",
        1719792000,
        root.as_ref(),
    )
    .unwrap();
    assert!(
        verify_chain_with_root(&vector("developer"), "other.dhttp.net", now, root.as_ref())
            .is_err()
    );
    let unrelated = crate::package::tests::packages::signing_root();
    assert!(
        verify_chain_with_root(
            &vector("developer"),
            "developer.dhttp.net",
            now,
            unrelated.as_ref()
        )
        .is_err()
    );
}
#[test]
fn offline_receipts_bind_historical_time_identity_bytes_and_signer() {
    let (_, bytes, proof) = fixture("history", "1.0.0", 'a', &[]);
    let at = proof["signature"]["verified_at"].as_i64().unwrap();
    verify_receipt(&proof, at).unwrap();
    for key in [
        "author",
        "name",
        "version",
        "package_sha256",
        "size",
        "author_owner_hash",
    ] {
        let mut bad = proof.clone();
        bad[key] = Value::Null;
        assert!(verify_receipt(&bad, at).is_err(), "{key}");
    }
    assert!(verify_receipt(&proof, at - 1).is_err());
    for key in ["payload", "signature", "certificates"] {
        let mut bad = proof.clone();
        bad["publication_receipt"][key] = Value::Null;
        assert!(verify_receipt(&bad, at).is_err(), "{key}");
    }
    let files = crate::package::read_package(&bytes).unwrap();
    let directory = tempfile::tempdir().unwrap();
    check_revocations(&files, directory.path(), &Value::Null, at).unwrap();
}
#[test]
fn signed_crls_are_cached_and_cannot_be_rolled_back_or_omitted() {
    let (_, bytes, _) = fixture("history", "1.0.0", 'a', &[]);
    let files = crate::package::read_package(&bytes).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let now = chrono::Utc::now().timestamp();
    let clean = crate::package::tests::packages::revocation_list(&bytes, 1, false, false);
    check_revocations(
        &files,
        directory.path(),
        &Value::from(STANDARD.encode(&clean)),
        now,
    )
    .unwrap();
    let revoked = crate::package::tests::packages::revocation_list(&bytes, 2, true, false);
    assert!(
        check_revocations(
            &files,
            directory.path(),
            &Value::from(STANDARD.encode(&revoked)),
            now
        )
        .is_err()
    );
    assert!(check_revocations(&files, directory.path(), &Value::Null, now).is_err());
    assert!(
        check_revocations(
            &files,
            directory.path(),
            &Value::from(STANDARD.encode(&clean)),
            now
        )
        .is_err()
    );
    let mut tampered = clean;
    *tampered.last_mut().unwrap() ^= 1;
    assert!(
        check_revocations(
            &files,
            directory.path(),
            &Value::from(STANDARD.encode(tampered)),
            now
        )
        .is_err()
    );
    let fresh = tempfile::tempdir().unwrap();
    let expired = crate::package::tests::packages::revocation_list(&bytes, 3, false, true);
    assert!(
        check_revocations(
            &files,
            fresh.path(),
            &Value::from(STANDARD.encode(expired)),
            now
        )
        .is_err()
    );
}
#[test]
fn entire_graph_is_rejected_before_installation_and_evidence_is_persisted_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let profile =
        dhttp_home::identity::IdentityProfile::try_from(dir.path().join("owner")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let mut sandbox = crate::sandbox::Sandbox::new(std::sync::Arc::new(
        crate::sandbox::WasmRuntime::new().unwrap(),
    ));
    let dep = fixture("dependency", "1.0.0", 'a', &[]);
    let root = fixture("root", "1.0.0", 'a', &["dependency"]);
    let mut bad = dep.clone();
    bad.2["signature"] = Value::Null;
    assert!(
        crate::package::files::install_packages(&profile, &[bad, root.clone()], &mut sandbox)
            .is_err()
    );
    assert!(
        crate::package::installed_packages(&profile, None)
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(!profile.join("app/root").exists() && !profile.join("app/dependency").exists());
    crate::package::files::install_packages(&profile, &[dep.clone(), root.clone()], &mut sandbox)
        .unwrap();
    let installed = crate::package::installed_packages(&profile, Some(&root.0)).unwrap();
    assert!(installed.get("signature_policy_version").is_none());
    assert_eq!(installed["author_owner_hash"], "a".repeat(64));
    assert_eq!(installed["package_sha256"], root.2["package_sha256"]);
    // Reused dependencies must still pass locally cached signed revocations.
    let mut revoked = dep;
    revoked.2["revocation_list"] = STANDARD
        .encode(crate::package::tests::packages::revocation_list(
            &revoked.1, 2, true, false,
        ))
        .into();
    assert!(
        crate::package::files::install_packages(&profile, &[revoked, root], &mut sandbox).is_err()
    );
    assert_eq!(
        crate::package::installed_packages(&profile, Some("alice.dhttp.net/root")).unwrap(),
        installed
    );
}
#[test]
fn owner_transfer_and_missing_old_evidence_cannot_seed_an_update_from_new_claims() {
    let dir = tempfile::tempdir().unwrap();
    let profile =
        dhttp_home::identity::IdentityProfile::try_from(dir.path().join("owner")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let mut sandbox = crate::sandbox::Sandbox::new(std::sync::Arc::new(
        crate::sandbox::WasmRuntime::new().unwrap(),
    ));
    let old = fixture("root", "1.0.0", 'a', &[]);
    crate::package::files::install_packages(&profile, std::slice::from_ref(&old), &mut sandbox)
        .unwrap();
    let takeover = fixture("root", "2.0.0", 'b', &[]);
    assert!(crate::package::files::install_packages(&profile, &[takeover], &mut sandbox).is_err());
    let db =
        crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    db.execute("UPDATE package SET author_owner_hash=NULL,package_sha256=NULL,signer_leaf_fingerprint_sha256=NULL",[]).unwrap();
    let mut legitimate = fixture("root", "2.0.0", 'a', &[]);
    assert!(
        crate::package::files::install_packages(&profile, &[legitimate.clone()], &mut sandbox)
            .is_err()
    );
    let mut forged = legitimate.clone();
    forged.2["_previous_release"] = old.2.clone();
    forged.2["_previous_release"]["publication_receipt"] = Value::Null;
    assert!(crate::package::files::install_packages(&profile, &[forged], &mut sandbox).is_err());
    legitimate.2["_previous_release"] = old.2;
    crate::package::files::install_packages(&profile, &[legitimate], &mut sandbox).unwrap();
}
#[test]
fn legacy_registry_migration_keeps_evidence_unknown_and_preserves_rows() {
    let dir = tempfile::tempdir().unwrap();
    let profile =
        dhttp_home::identity::IdentityProfile::try_from(dir.path().join("owner")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let db =
        crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    db.execute_batch("DROP TABLE package_dependency; DROP TABLE package;")
        .unwrap();
    db.execute_batch(include_str!(
        "../../../../src/package/sql/compat/v3/schema.sql"
    ))
    .unwrap();
    db.execute("INSERT INTO package(name,author,version,has_lib,has_app) VALUES('legacy','alice.dhttp.net','1.0.0',0,1)",[]).unwrap();
    drop(db);
    crate::setup::load_server_config(&profile).unwrap();
    let row = crate::package::installed_packages(&profile, Some("alice.dhttp.net/legacy")).unwrap();
    for key in [
        "author_owner_hash",
        "package_sha256",
        "signer_leaf_fingerprint_sha256",
    ] {
        assert!(row[key].is_null());
    }
    assert_eq!(row["version"], "1.0.0");
}

#[test]
fn actual_jade_signed_bytes_pass_the_same_installation_and_registration_gate() {
    // Public fixture built by Jade with SIGNING_HOME containing no ssl directory.
    // Registry transport and CertServer status are explicit mocks here; this is
    // not a claim that a real Registry publication has occurred.
    let bytes = include_bytes!("../../../fixtures/code-signing/jade-developer.waz").to_vec();
    assert_eq!(
        format!("{:x}", Sha256::digest(&bytes)),
        "ec726212c8c8be0081360eddf44a60d06e76a6ba1958d65bca3e16532dddab8e"
    );
    let mut proof: Value = serde_json::from_slice(include_bytes!(
        "../../../fixtures/code-signing/jade-registry-proof.mock.json"
    ))
    .unwrap();
    crate::package::tests::packages::sign_publication(&mut proof);
    let root = tempfile::tempdir().unwrap();
    let profile =
        dhttp_home::identity::IdentityProfile::try_from(root.path().join("installer")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let mut sandbox = crate::sandbox::Sandbox::new(std::sync::Arc::new(
        crate::sandbox::WasmRuntime::new().unwrap(),
    ));
    let id = "developer.dhttp.net/developer-jade-smoke";
    crate::package::files::install_packages(
        &profile,
        &[(id.into(), bytes, proof.clone())],
        &mut sandbox,
    )
    .unwrap();
    let installed = crate::package::installed_packages(&profile, Some(id)).unwrap();
    assert_eq!(installed["version"], "0.0.1");
    assert_eq!(installed["author_owner_hash"], proof["author_owner_hash"]);
    assert_eq!(installed["package_sha256"], proof["package_sha256"]);
    assert_eq!(
        installed["signer_leaf_fingerprint_sha256"],
        proof["signature"]["leaf_fingerprint_sha256"]
    );
    assert!(installed.get("signature_policy_version").is_none());
    assert_eq!(
        std::fs::read(profile.join("app/developer-jade-smoke/index.html")).unwrap(),
        b"<h1>Developer-signed Jade integration</h1>\n"
    );
}

#[test]
fn expired_developer_certificates_install_only_with_an_authentic_historical_receipt() {
    let bytes = include_bytes!("../../../fixtures/code-signing/expired.waz");
    assert!(crate::package::read_package(bytes).is_err());
    let proof = crate::package::tests::packages::release_proof_at(bytes, 1719792000);
    crate::package::store::verify_release(
        bytes,
        &proof,
        "developer.dhttp.net",
        "test-signing",
        "1.0.0",
    )
    .unwrap();
    let mut changed = proof;
    changed["signature"]["verified_at"] = chrono::Utc::now().timestamp().into();
    crate::package::tests::packages::sign_publication(&mut changed);
    assert!(
        crate::package::store::verify_release(
            bytes,
            &changed,
            "developer.dhttp.net",
            "test-signing",
            "1.0.0"
        )
        .is_err()
    );
}

#[test]
fn unified_chain_preserves_offline_verification_and_rejects_partial_usages() {
    let fixtures =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/unified-developer");
    let read = |name: &str| pem(&std::fs::read(fixtures.join(name)).unwrap());
    let root = read("root.cert.pem");
    let chain = vec![read("developer.cert.pem"), read("issuer.cert.pem")];
    let at = chrono::Utc::now().timestamp();
    verify_chain_with_root(&chain, "developer.dhttp.net", at, root.as_ref()).unwrap();
    let partial = vec![read("partial.cert.pem"), read("issuer.cert.pem")];
    assert!(verify_chain_with_root(&partial, "developer.dhttp.net", at, root.as_ref()).is_err());
    assert!(verify_chain_with_root(&chain, "other.dhttp.net", at, root.as_ref()).is_err());
}
