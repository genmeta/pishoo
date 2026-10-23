use std::sync::Arc;

use super::{super::WasmRuntime, *};

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
    let mut metadata = serde_json::json!({"id":"alice.dhttp.net/note","publisher":publisher,"name":"note","version":version,
        "author_owner_hash":owner.to_string().repeat(64),"size":bytes.len(),"package_sha256":format!("{:x}",Sha256::digest(&bytes)),
        "openapi":document,"signer_certificates":certificates});
    let package = read_package(&bytes).unwrap();
    let fields = package_metadata(&package).unwrap();
    for (key, value) in fields.as_object().unwrap() {
        metadata[key] = value.clone();
    }
    metadata["author"] = fields["publisher"].clone();
    metadata.as_object_mut().unwrap().remove("publisher");
    metadata["dependencies"] = fields["dependencies"].as_array().unwrap().iter().map(|dependency| serde_json::json!({
        "author":dependency["publisher"],"name":dependency["name"],"version":dependency["version"]
    })).collect::<Vec<_>>().into();
    metadata["openapi"] =
        serde_json::from_slice(&package_file(&package, "/metadata/openapi.json").unwrap()).unwrap();
    (bytes, metadata)
}

#[test]
fn authenticated_release_rejects_tampered_bytes_metadata_signers_and_identity() {
    let (bytes, metadata) = release('a', "1.2.3");
    verify_release(&bytes, &metadata, "alice.dhttp.net", "note", "1.2.3").unwrap();
    for key in [
        "package_sha256",
        "author_owner_hash",
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
fn complete_ids_conflict_with_legacy_names_and_reject_other_authors() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.join("lib")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let (bytes, _) = release('a', "1.2.3");
    install_lib(&profile, "note", &bytes, &runtime).unwrap();
    let id = "alice.dhttp.net/note";
    dbg!(super::super::installed_packages(&profile, None).unwrap());
    assert!(install_lib(&profile, id, &bytes, &runtime).is_err());
    super::super::remove_lib(&profile, "note").unwrap();
    install_lib(&profile, id, &bytes, &runtime).unwrap();
    assert_eq!(
        super::super::installed_libs(&profile, None)
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let mut sandbox = super::super::Sandbox::new(runtime.clone());
    sandbox.load_libs(&profile).unwrap();
    assert!(!sandbox.libs.contains_key("note") && sandbox.libs.contains_key(id));
    assert!(profile.join(".volume/note").is_dir());
    assert!(!profile.join("db/alice.dhttp.net").exists());
    let (rotated, _) = release('a', "1.2.4");
    install_lib(&profile, id, &rotated, &runtime).unwrap();
    let (takeover, _) = release('b', "2.0.0");
    install_lib(&profile, id, &takeover, &runtime).unwrap();
    let snapshot = super::super::files::disk_bytes(
        &profile,
        &super::super::files::lib_root(&profile).unwrap(),
        id,
    )
    .unwrap();
    assert!(
        read_package(&snapshot).is_err(),
        "installed runtime snapshot has no release proof"
    );
    let loaded = Lib::load(
        runtime.clone(),
        id.into(),
        &snapshot,
        &profile.join(".volume").join(id.rsplit('/').next().unwrap()),
    )
    .unwrap();
    assert_eq!(loaded.openapi.info.version, "2.0.0");
    assert!(install_lib(&profile, "bob.dhttp.net/note", &bytes, &runtime).is_err());
    super::super::remove_lib(&profile, id).unwrap();
    super::super::remove_lib(&profile, id).unwrap();
    assert!(!profile.join("lib/note.wasm").exists());
    assert!(profile.join(".volume/note").is_dir());
    let outside = root.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, profile.join("lib/note.wasm")).unwrap();
    assert!(install_lib(&profile, id, &bytes, &runtime).is_err());
    super::super::remove_lib(&profile, id).unwrap();
    assert_eq!(std::fs::read_dir(outside).unwrap().count(), 0);
}

#[test]
fn frontend_release_proof_binds_package_dependencies_without_runtime_projection() {
    let (bytes, mut metadata) = release('a', "1.0.0");
    for key in ["host", "entry", "openapi", "has_lib", "has_app", "kind"] {
        metadata.as_object_mut().unwrap().remove(key);
    }
    verify_release(&bytes, &metadata, "alice.dhttp.net", "note", "1.0.0").unwrap();
    metadata["dependencies"] =
        serde_json::json!([{"publisher":"alice.dhttp.net","name":"other","version":"1.0.0"}]);
    assert!(verify_release(&bytes, &metadata, "alice.dhttp.net", "note", "1.0.0").is_err());
}

#[test]
fn dependency_tree_orders_transitive_libs_and_rejects_cycles() {
    use super::super::tests::{package_fixture, publisher};
    let authority = publisher("alice.dhttp.net", 'a');
    let package = |name: &str, names: &[&str]| {
        let dependencies=names.iter().map(|name|serde_json::json!({"publisher":"alice.dhttp.net","name":name,"version":"1.0.0"})).collect::<Vec<_>>();
        let document = serde_json::json!({"openapi":"3.1.0","info":{"title":name,"description":"Dependency graph fixture","version":"1.0.0","license":{"name":"MIT"}},"x-pishoo":{"kind":"lib","publisher":"alice.dhttp.net","name":name,"host":{},"dependencies":dependencies},"paths":{"/upload":{"post":{"operationId":"upload","summary":"Upload"}}}});
        let bytes = package_fixture(
            &document,
            include_bytes!("../../fixtures/wasi-http-read-request-then-respond.wasm"),
            webc::v3::write::Directory::default(),
            &authority,
        );
        (
            "lib".into(),
            format!("alice.dhttp.net/{name}"),
            bytes,
            false,
        )
    };
    let ordered = order_releases(vec![
        package("parent", &["first", "second"]),
        package("second", &["shared"]),
        package("first", &["shared"]),
        package("shared", &[]),
    ])
    .unwrap();
    let ids = ordered
        .iter()
        .map(|(_, id, _, _)| id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids.first(), Some(&"alice.dhttp.net/shared"));
    assert_eq!(ids.last(), Some(&"alice.dhttp.net/parent"));
    assert_eq!(ids.len(), 4);
    assert!(
        order_releases(vec![
            package("first", &["second"]),
            package("second", &["first"])
        ])
        .is_err()
    );
    assert!(order_releases(vec![package("parent", &["missing"])]).is_err());
    let (kind, id, bytes, reused) = package("parent", &["first", "second"]);
    let container = read_package(&bytes).unwrap();
    let document: Value = serde_json::from_slice(
        &super::super::package_file(&container, "/metadata/openapi.json").unwrap(),
    )
    .unwrap();
    let signature = container
        .get_volume("pishoo-signature")
        .unwrap()
        .read_file("/signature.json")
        .unwrap()
        .0;
    let envelope: Value = serde_json::from_slice(&signature).unwrap();
    let mut metadata = serde_json::json!({"kind":kind,"id":id,"publisher":"alice.dhttp.net","name":"parent","version":"1.0.0","author_owner_hash":"a".repeat(64),"package_sha256":format!("{:x}",Sha256::digest(&bytes)),"size":bytes.len(),"openapi":document,"dependencies":document["x-pishoo"]["dependencies"],"signer_certificates":envelope["certificates"]});
    let fields = package_metadata(&container).unwrap();
    for (key, value) in fields.as_object().unwrap() {
        metadata[key] = value.clone();
    }
    metadata["author"] = fields["publisher"].clone();
    metadata.as_object_mut().unwrap().remove("publisher");
    metadata["dependencies"] = fields["dependencies"].as_array().unwrap().iter().map(|dependency| serde_json::json!({
        "author":dependency["publisher"],"name":dependency["name"],"version":dependency["version"]
    })).collect::<Vec<_>>().into();
    assert!(!reused);
    verify_release(&bytes, &metadata, "alice.dhttp.net", "parent", "1.0.0").unwrap();
    let mut altered = metadata;
    altered["dependencies"] = serde_json::json!([]);
    assert!(verify_release(&bytes, &altered, "alice.dhttp.net", "parent", "1.0.0").is_err());
}

#[test]
#[ignore = "requires recorded real std publication packages and store metadata"]
fn published_std_release_proofs_match_signed_packages() {
    let root = std::path::PathBuf::from(std::env::var("PISHOO_PUBLISHED_STD_RECORDS").unwrap());
    for (kind, name) in [
        ("lib", "profile"),
        ("lib", "message"),
        ("lib", "calendar"),
        ("app", "workspace"),
        ("app", "calendar"),
    ] {
        let bytes = std::fs::read(
            root.join("packages")
                .join(format!("{name}-0.1.0-{kind}.webc")),
        )
        .unwrap();
        let metadata: Value = serde_json::from_slice(
            &std::fs::read(
                root.join("log")
                    .join(format!("diagnose-{name}-{kind}-store-metadata.stdout")),
            )
            .unwrap(),
        )
        .unwrap();
        verify_release(&bytes, &metadata, "std.dhttp.net", name, "0.1.0")
            .unwrap_or_else(|error| panic!("{kind} {name}: {error}"));
        println!("verified published {kind} std.dhttp.net/{name}@0.1.0");
    }
}

fn order_releases(
    resources: Vec<(String, String, Vec<u8>, bool)>,
) -> Result<Vec<(String, String, Vec<u8>, bool)>> {
    Ok(order_packages(
        resources
            .into_iter()
            .map(|(_, id, bytes, _)| (id, bytes))
            .collect(),
    )?
    .into_iter()
    .map(|(id, bytes)| ("pkg".into(), id, bytes, false))
    .collect())
}
