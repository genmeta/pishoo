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
        "publisher_owner_hash":owner.to_string().repeat(64),"size":bytes.len(),"package_sha256":format!("{:x}",Sha256::digest(&bytes)),
        "openapi":document,"signer_certificates":certificates});
    let package = read_package(&bytes).unwrap();
    let fields = package_metadata(&package).unwrap();
    for (key, value) in fields.as_object().unwrap() {
        metadata[key] = value.clone();
    }
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
        "publisher_owner_hash",
        "openapi",
        "host",
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
    std::fs::create_dir_all(profile.join("libs")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let (bytes, _) = release('a', "1.2.3");
    install_lib(&profile, "note", &bytes, &runtime).unwrap();
    let id = "alice.dhttp.net/note";
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
    assert!(profile.join("db/note").is_dir());
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
        &profile.join("db").join(id.rsplit('/').next().unwrap()),
    )
    .unwrap();
    assert_eq!(loaded.openapi.info.version, "2.0.0");
    assert!(install_lib(&profile, "bob.dhttp.net/note", &bytes, &runtime).is_err());
    super::super::remove_lib(&profile, id).unwrap();
    super::super::remove_lib(&profile, id).unwrap();
    assert!(!profile.join("libs/note.wasm").exists());
    assert!(profile.join("db/note").is_dir());
    let outside = root.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, profile.join("libs/note.wasm")).unwrap();
    assert!(install_lib(&profile, id, &bytes, &runtime).is_err());
    super::super::remove_lib(&profile, id).unwrap();
    assert_eq!(std::fs::read_dir(outside).unwrap().count(), 0);
}

#[test]
fn frontend_release_proof_binds_kind_and_exact_backend_dependencies() {
    use super::super::tests::{package_fixture, publisher};
    let authority = publisher("alice.dhttp.net", 'a');
    let document = serde_json::json!({"openapi":"3.1.0","info":{"title":"Frontend","version":"1.0.0","description":"Separate frontend","license":{"name":"MIT"}},"x-pishoo":{"kind":"app","publisher":"alice.dhttp.net","name":"note","entry":"/","host":{},"dependencies":[{"publisher":"alice.dhttp.net","name":"note","version":"1.2.3"}]},"paths":{"/":{"get":{"operationId":"page","summary":"Page","x-pishoo":{"static-file":"/public/index.html"}},"head":{"operationId":"headPage","summary":"Page header","x-pishoo":{"static-file":"/public/index.html"}}}}});
    let public = webc::v3::write::Directory::new(
        std::collections::BTreeMap::from([(
            "index.html".parse().unwrap(),
            b"page".to_vec().into(),
        )]),
        webc::v3::Timestamps::default(),
    );
    let bytes = package_fixture(&document, &[], public, &authority);
    let mut metadata = serde_json::json!({"kind":"app","id":"alice.dhttp.net/note","publisher":"alice.dhttp.net","name":"note","version":"1.0.0","publisher_owner_hash":"a".repeat(64),"size":bytes.len(),"package_sha256":format!("{:x}",Sha256::digest(&bytes)),"openapi":document,"dependencies":document["x-pishoo"]["dependencies"],"signer_certificates":authority.certificates().iter().map(|c| STANDARD.encode(c.as_ref())).collect::<Vec<_>>()});
    metadata.as_object_mut().unwrap().remove("openapi");
    let fields = package_metadata(&read_package(&bytes).unwrap()).unwrap();
    for (key, value) in fields.as_object().unwrap() {
        metadata[key] = value.clone();
    }
    verify_release(&bytes, &metadata, "alice.dhttp.net", "note", "1.0.0").unwrap();
    // The store projects App host capabilities as {}, while the signed App
    // manifest must omit host entirely.
    metadata["host"] = serde_json::json!({});
    verify_release(&bytes, &metadata, "alice.dhttp.net", "note", "1.0.0").unwrap();
    for host in [
        serde_json::json!({"private-storage": "Forged capability"}),
        serde_json::json!([]),
        serde_json::json!("invalid"),
    ] {
        let mut wrong = metadata.clone();
        wrong["host"] = host;
        assert!(verify_release(&bytes, &wrong, "alice.dhttp.net", "note", "1.0.0").is_err());
    }
    let mut wrong = metadata.clone();
    wrong["kind"] = serde_json::json!("lib");
    assert!(verify_release(&bytes, &wrong, "alice.dhttp.net", "note", "1.0.0").is_err());
    wrong = metadata;
    wrong["dependencies"][0]["version"] = serde_json::json!("2.0.0");
    assert!(verify_release(&bytes, &wrong, "alice.dhttp.net", "note", "1.0.0").is_err());
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
    let mut metadata = serde_json::json!({"kind":kind,"id":id,"publisher":"alice.dhttp.net","name":"parent","version":"1.0.0","publisher_owner_hash":"a".repeat(64),"package_sha256":format!("{:x}",Sha256::digest(&bytes)),"size":bytes.len(),"openapi":document,"dependencies":document["x-pishoo"]["dependencies"],"signer_certificates":envelope["certificates"]});
    let fields = package_metadata(&container).unwrap();
    for (key, value) in fields.as_object().unwrap() {
        metadata[key] = value.clone();
    }
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
                root.join("logs")
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

#[test]
fn installed_dependency_reuse_uses_registered_identity_and_version_only() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.join("libs")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let (source, _) = release('a', "1.2.3");
    let id = "alice.dhttp.net/note";
    install_lib(&profile, id, &source, &runtime).unwrap();
    assert!(can_reuse_lib(&profile, id, "1.2.3").unwrap());
    let path = profile.join("libs/note.wasm");
    let mut changed = std::fs::read(&path).unwrap();
    section(&mut changed, "local-note", b"locally edited payload");
    std::fs::write(&path, &changed).unwrap();
    assert!(can_reuse_lib(&profile, id, "1.2.3").unwrap());
    std::fs::remove_file(&path).unwrap();
    assert!(
        can_reuse_lib(&profile, id, "1.2.3").unwrap(),
        "reuse eligibility does not inspect local files"
    );
    assert!(!can_reuse_lib(&profile, "bob.dhttp.net/note", "1.2.3").unwrap());
    assert!(!can_reuse_lib(&profile, id, "2.0.0").unwrap());
}

#[test]
fn loaded_legacy_dependency_is_reinstalled_before_it_becomes_reusable() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    std::fs::create_dir_all(profile.join("libs")).unwrap();
    std::fs::create_dir_all(profile.join("apps")).unwrap();
    let db = rusqlite::Connection::open(profile.config_db_path()).unwrap();
    db.execute_batch("PRAGMA foreign_keys=OFF;
        DROP TABLE lib_capability_operations; DROP TABLE lib_capabilities;
        DROP TABLE app_dependencies; DROP TABLE lib_dependencies; DROP TABLE libs; DROP TABLE apps;
        CREATE TABLE libs(id TEXT PRIMARY KEY NOT NULL,name TEXT UNIQUE NOT NULL,version TEXT NOT NULL);
        CREATE TABLE apps(id TEXT PRIMARY KEY NOT NULL,name TEXT UNIQUE NOT NULL,version TEXT NOT NULL);
        CREATE TABLE app_dependencies(app_id TEXT REFERENCES apps(id) ON DELETE CASCADE,lib_id TEXT REFERENCES libs(id) ON DELETE RESTRICT,version TEXT,PRIMARY KEY(app_id,lib_id));
        CREATE TABLE lib_dependencies(lib_id TEXT REFERENCES libs(id) ON DELETE CASCADE,dependency_id TEXT REFERENCES libs(id) ON DELETE RESTRICT,version TEXT,PRIMARY KEY(lib_id,dependency_id));
        CREATE TABLE lib_capabilities(lib_id TEXT REFERENCES libs(id) ON DELETE CASCADE,capability_id TEXT,title TEXT,PRIMARY KEY(lib_id,capability_id));
        CREATE TABLE lib_capability_operations(lib_id TEXT,capability_id TEXT,method TEXT,path TEXT,PRIMARY KEY(lib_id,capability_id,method,path),FOREIGN KEY(lib_id,capability_id) REFERENCES lib_capabilities(lib_id,capability_id) ON DELETE CASCADE);").unwrap();
    let id = "alice.dhttp.net/note";
    let (source, _) = release('a', "1.2.3");
    db.execute("INSERT INTO libs VALUES(?1,'note','1.2.3')", [id])
        .unwrap();
    let artifact = profile.join("lib/alice.dhttp.net/note/lib.webc");
    std::fs::create_dir_all(artifact.parent().unwrap()).unwrap();
    std::fs::write(&artifact, &source).unwrap();
    let legacy_data = profile.join("db/alice.dhttp.net/note");
    std::fs::create_dir_all(&legacy_data).unwrap();
    std::fs::write(legacy_data.join("keep.db"), b"existing business data").unwrap();
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let mut sandbox = Sandbox::new(runtime.clone());
    sandbox.load_libs(&profile).unwrap();
    assert_eq!(sandbox.libs[id].openapi.info.version, "1.2.3");
    assert!(!profile.join("db/note").exists());
    assert_eq!(
        std::fs::read(legacy_data.join("keep.db")).unwrap(),
        b"existing business data"
    );
    assert!(!can_reuse_lib(&profile, id, "1.2.3").unwrap());
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM pragma_table_info('libs') WHERE name='id'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        1,
        "eligibility check must not migrate the catalog"
    );
    install_lib(&profile, id, &source, &runtime).unwrap();
    assert!(can_reuse_lib(&profile, id, "1.2.3").unwrap());
    assert!(!can_reuse_lib(&profile, id, "1.2.4").unwrap());
    let snapshot = super::super::files::disk_bytes(
        &profile,
        &super::super::files::lib_root(&profile).unwrap(),
        id,
    )
    .unwrap();
    let installed = Lib::load(
        runtime,
        id.into(),
        &snapshot,
        &profile.join("db").join(id.rsplit('/').next().unwrap()),
    )
    .unwrap();
    assert_eq!(installed.openapi.info.version, "1.2.3");
    assert!(!profile.join("db/note").exists());
    assert_eq!(
        std::fs::read(legacy_data.join("keep.db")).unwrap(),
        b"existing business data"
    );
    db.execute(
        "UPDATE libs SET author='bob.dhttp.net' WHERE name='note'",
        [],
    )
    .unwrap();
    assert!(!can_reuse_lib(&profile, id, "1.2.3").unwrap());
    assert_eq!(std::fs::read(artifact).unwrap(), source);
}
