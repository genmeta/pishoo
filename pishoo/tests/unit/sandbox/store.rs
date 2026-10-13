use std::sync::Arc;

use super::super::WasmRuntime;
use super::*;

fn section(bytes: &mut Vec<u8>, name: &str, data: &[u8]) {
    fn leb(mut value: usize, bytes: &mut Vec<u8>) {
        loop {
            let byte = (value & 127) as u8;
            value >>= 7;
            bytes.push(byte | if value != 0 { 128 } else { 0 });
            if value == 0 {
                break;
            }
        }
    }
    let mut content = Vec::new();
    leb(name.len(), &mut content);
    content.extend_from_slice(name.as_bytes());
    content.extend_from_slice(data);
    bytes.push(0);
    leb(content.len(), bytes);
    bytes.extend(content);
}

fn release(owner: char, version: &str) -> (Vec<u8>, Value) {
    let publisher = "alice.dhttp.net";
    let mut params = rcgen::CertificateParams::new(vec![publisher.into()]).unwrap();
    let ski = format!("1:0:{}", owner.to_string().repeat(64));
    let mut der = vec![4, ski.len() as u8];
    der.extend_from_slice(ski.as_bytes());
    params
        .custom_extensions
        .push(rcgen::CustomExtension::from_oid_content(
            &[2, 5, 29, 14],
            der,
        ));
    let key = rcgen::KeyPair::generate().unwrap();
    let certificate = params.self_signed(&key).unwrap();
    let local = dhttp::LocalAuthority::new(
        &qtls::default_provider(),
        Arc::from(publisher),
        vec![certificate.der().clone()],
        qtls::PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
        vec![1],
    )
    .unwrap();
    let document = serde_json::json!({"openapi":"3.1.0","info":{"title":"Store test","version":version,"description":"Test","license":{"name":"MIT"}},
        "x-pishoo":{"publisher":publisher,"name":"note","host":{}},
        "paths":{"/upload":{"post":{"operationId":"upload","summary":"Upload","responses":{"200":{"description":"ok"}}}}}});
    let bytes = super::super::tests::package_fixture(
        &document,
        include_bytes!("../../fixtures/wasi-http-read-request-then-respond.wasm"),
        webc::v3::write::Directory::default(),
        &local,
    );
    let certificates = local
        .certificates()
        .iter()
        .map(|c| STANDARD.encode(c.as_ref()))
        .collect::<Vec<_>>();
    let metadata = serde_json::json!({"id":"alice.dhttp.net/note","publisher":publisher,"name":"note","version":version,
        "publisher_owner_hash":owner.to_string().repeat(64),"size":bytes.len(),"package_sha256":format!("{:x}",Sha256::digest(&bytes)),
        "openapi":document,"signer_certificates":certificates});
    (bytes, metadata)
}

#[test]
fn authenticated_release_rejects_tampered_bytes_metadata_signers_and_identity() {
    let (bytes, metadata) = release('a', "1.2.3");
    verify_release(&bytes, &metadata, "alice.dhttp.net", "note", "1.2.3").unwrap();
    for key in [
        "package_sha256",
        "publisher_owner_hash",
        "openapi",
        "signer_certificates",
        "size",
    ] {
        let mut bad = metadata.clone();
        bad[key] = Value::Null;
        assert!(
            verify_release(&bytes, &bad, "alice.dhttp.net", "note", "1.2.3").is_err(),
            "{key}"
        );
    }
    assert!(verify_release(&bytes, &metadata, "bob.dhttp.net", "note", "1.2.3").is_err());
    assert!(verify_release(&bytes, &metadata, "alice.dhttp.net", "other", "1.2.3").is_err());
    assert!(verify_release(&bytes, &metadata, "alice.dhttp.net", "note", "1.2.4").is_err());
    let mut tampered = bytes.clone();
    let index = tampered
        .windows(b"Store test".len())
        .position(|b| b == b"Store test")
        .unwrap();
    tampered[index] = b'X';
    let mut forged = metadata.clone();
    forged["package_sha256"] = format!("{:x}", Sha256::digest(&tampered)).into();
    assert!(verify_release(&tampered, &forged, "alice.dhttp.net", "note", "1.2.3").is_err());
    assert!(read_package(&tampered).is_err());
    let mut suffix = bytes.clone();
    section(&mut suffix, "extra", b"suffix");
    assert!(read_package(&suffix).is_err());
}

#[test]
fn complete_ids_keep_legacy_resources_separate_and_refuse_publisher_takeover() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.join("lib")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let (bytes, _) = release('a', "1.2.3");
    install_lib(&profile, "note", &bytes, &runtime).unwrap();
    let id = "alice.dhttp.net/note";
    install_lib(&profile, id, &bytes, &runtime).unwrap();
    assert_eq!(
        super::super::installed_libs(&profile, None)
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let mut sandbox = super::super::Sandbox::new(runtime.clone());
    sandbox.load_libs(&profile).unwrap();
    assert!(sandbox.libs.contains_key("note") && sandbox.libs.contains_key(id));
    assert!(profile.join("db/note").is_dir());
    assert!(profile.join("db/alice.dhttp.net/note").is_dir());
    let (rotated, _) = release('a', "1.2.4");
    install_lib(&profile, id, &rotated, &runtime).unwrap();
    let (takeover, _) = release('b', "2.0.0");
    assert!(install_lib(&profile, id, &takeover, &runtime).is_err());
    assert_eq!(
        std::fs::read(profile.join("lib/alice.dhttp.net/note/lib.webc")).unwrap(),
        rotated
    );
    assert!(install_lib(&profile, "bob.dhttp.net/note", &bytes, &runtime).is_err());
    super::super::remove_lib(&profile, id).unwrap();
    super::super::remove_lib(&profile, id).unwrap();
    assert!(profile.join("lib/note/lib.webc").is_file());
    assert!(profile.join("db/alice.dhttp.net/note").is_dir());
    let outside = root.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::remove_dir(profile.join("lib/alice.dhttp.net")).unwrap();
    std::os::unix::fs::symlink(&outside, profile.join("lib/alice.dhttp.net")).unwrap();
    assert!(install_lib(&profile, id, &bytes, &runtime).is_err());
    assert!(super::super::remove_lib(&profile, id).is_err());
    assert_eq!(std::fs::read_dir(outside).unwrap().count(), 0);
}
