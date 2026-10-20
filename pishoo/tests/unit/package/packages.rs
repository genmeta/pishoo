use std::{collections::BTreeMap, sync::Arc};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use webc::v3::{
    SignatureAlgorithm, Timestamps,
    write::{Directory, FileEntry, Writer},
};

use super::*;

pub(crate) fn publisher(name: &str, owner: char) -> dhttp::LocalAuthority {
    let mut params = rcgen::CertificateParams::new(vec![name.into()]).unwrap();
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
    dhttp::LocalAuthority::new(
        &qtls::default_provider(),
        Arc::from(name),
        vec![certificate.der().clone()],
        qtls::PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
        vec![1],
    )
    .unwrap()
}

pub(crate) fn package_fixture(
    document: &Value,
    backend: &[u8],
    public: Directory<'_>,
    authority: &dhttp::LocalAuthority,
) -> Vec<u8> {
    // Test build input only. Production has one signed manifest authority.
    let app = document["x-pishoo"]["kind"] == "app";
    let mut api = document.clone();
    let mut annotation = serde_json::json!({"kind":if app { "app" } else { "lib" },"title":document["info"]["title"],
        "changelog":document["x-pishoo"].get("changelog").cloned().unwrap_or_else(||"Test release; data unchanged".into()),
        "dependencies":document["x-pishoo"].get("dependencies").cloned().unwrap_or_else(||json!([]))});
    if app {
        annotation["entry"] = document["x-pishoo"]
            .get("entry")
            .cloned()
            .unwrap_or_else(|| "/".into());
    } else {
        annotation["host"] = document["x-pishoo"]
            .get("host")
            .cloned()
            .unwrap_or_else(|| json!({}));
    }
    if let Some(extension) = api["x-pishoo"].as_object_mut() {
        extension.retain(|key, _| key == "access-groups");
    }
    let volume = if app { "public" } else { "metadata" };
    let mut manifest: webc::metadata::Manifest = serde_json::from_value(json!({
        "package":{"wapm":{"name":format!("{}/{}",document["x-pishoo"]["publisher"].as_str().unwrap(),document["x-pishoo"]["name"].as_str().unwrap()),
            "version":document["info"]["version"],"description":document["info"]["description"],"license":document["info"]["license"]["name"]},
            "pishoo":annotation,"fs":[{"volume_name":volume,"mount_path":format!("/{volume}")}]},
        "atoms":{"backend":{"kind":"https://webc.org/kind/wasm","signature":format!("sha256:{}",STANDARD.encode(Sha256::digest(backend)))}}
    })).unwrap();
    if app {
        manifest.atoms.clear();
    }
    let atoms = || {
        if app {
            BTreeMap::new()
        } else {
            BTreeMap::from([(
                "backend".parse().unwrap(),
                FileEntry::borrowed(backend, Timestamps::default()),
            )])
        }
    };
    let mut writer = Writer::default()
        .write_manifest(&manifest)
        .unwrap()
        .write_atoms(atoms())
        .unwrap();
    if app {
        writer.write_volume("public", public).unwrap();
    } else {
        // An obsolete combined fixture is rejected rather than accepted as a Lib.
        if !public.children.is_empty() {
            writer.write_volume("public", public).unwrap();
        }
        writer
            .write_volume(
                "metadata",
                Directory::new(
                    BTreeMap::from([(
                        "openapi.json".parse().unwrap(),
                        serde_json::to_vec(&api).unwrap().into(),
                    )]),
                    Timestamps::default(),
                ),
            )
            .unwrap();
    }
    let unsigned = writer.finish(SignatureAlgorithm::None).unwrap();
    let container =
        webc::Container::from_bytes_and_version(unsigned.clone(), webc::Version::V3).unwrap();
    let reader = webc::v3::read::OwnedReader::parse(unsigned.clone()).unwrap();
    let index = reader.index();
    let mut hash = Sha256::new();
    hash.update(b"\0webc003");
    for entry in std::iter::once(&index.manifest)
        .chain(std::iter::once(&index.atoms))
        .chain(index.volumes.values())
    {
        hash.update(&unsigned[entry.span.start..entry.span.end()]);
    }
    let mut input = b"pishoo-webc-signature-v1\0".to_vec();
    input.extend_from_slice(&hash.finalize());
    let signature = authority
        .sign(qtls::SignatureScheme::ECDSA_NISTP256_SHA256, &input)
        .unwrap();
    let envelope = json!({"version":1,"certificates":authority.certificates().iter().map(|c|STANDARD.encode(c.as_ref())).collect::<Vec<_>>(),"signature":STANDARD.encode(signature)});
    writer = Writer::default()
        .write_raw_manifest(
            unsigned[index.manifest.span.start + 41..index.manifest.span.end()].to_vec(),
        )
        .write_atoms(atoms())
        .unwrap();
    for (name, volume) in container.volumes() {
        writer
            .write_volume(&name, copy_directory(&volume, "/"))
            .unwrap();
    }
    writer
        .with_volume(
            "pishoo-signature",
            Directory::new(
                BTreeMap::from([(
                    "signature.json".parse().unwrap(),
                    serde_json::to_vec(&envelope).unwrap().into(),
                )]),
                Timestamps::default(),
            ),
        )
        .unwrap()
        .finish(SignatureAlgorithm::None)
        .unwrap()
        .to_vec()
}

fn copy_directory(volume: &webc::Volume, path: &str) -> Directory<'static> {
    use webc::v3::write::DirEntry;
    let timestamps = |value: Option<webc::Timestamps>| Timestamps {
        modified: std::time::UNIX_EPOCH
            + Duration::from_nanos(value.unwrap_or_default().modified()),
    };
    let children = volume
        .read_dir(path)
        .unwrap()
        .into_iter()
        .map(|(name, _, metadata)| {
            let child = format!("{}/{}", path.trim_end_matches('/'), name);
            let entry = match metadata {
                webc::Metadata::Dir { .. } => DirEntry::Dir(copy_directory(volume, &child)),
                webc::Metadata::File {
                    timestamps: value, ..
                } => DirEntry::File(FileEntry::owned(
                    volume.read_file(child.as_str()).unwrap().0.to_vec(),
                    timestamps(value),
                )),
                _ => panic!("unexpected fixture entry"),
            };
            (name, entry)
        })
        .collect();
    let stamp = match volume.metadata(path).unwrap() {
        webc::Metadata::Dir { timestamps: value } => timestamps(value),
        _ => Timestamps::default(),
    };
    Directory::new(children, stamp)
}

// Preserve existing execution fixtures, extracting their legacy test-only OpenAPI
// into a signed WEBC so the real loader always sees the production package format.
pub(crate) fn package_component(bytes: &[u8]) -> Vec<u8> {
    let mut backend = bytes[..8].to_vec();
    let mut document = None;
    let mut depth = 0;
    let mut start = 8;
    for payload in wasmparser::Parser::new(0).parse_all(bytes) {
        let payload = payload.unwrap();
        if depth == 0
            && let Some((_, range)) = payload.as_section()
        {
            let end = range.end as usize;
            match &payload {
                wasmparser::Payload::CustomSection(s) if s.name() == "pishoo:openapi" => {
                    document = Some(serde_json::from_slice::<Value>(s.data()).unwrap())
                }
                wasmparser::Payload::CustomSection(s) if s.name() == "pishoo:signature" => {}
                _ => backend.extend_from_slice(&bytes[start..end]),
            }
            start = end;
        }
        match payload {
            wasmparser::Payload::ModuleSection { .. }
            | wasmparser::Payload::ComponentSection { .. } => depth += 1,
            wasmparser::Payload::End(_) if depth > 0 => depth -= 1,
            _ => {}
        }
    }
    let mut doc = document.unwrap();
    if !doc["info"]["version"].as_str().unwrap().contains('.') {
        let version = doc["info"]["version"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap_or(1);
        doc["info"]["version"] = format!("{version}.0.0").into();
    }
    if doc["x-pishoo"].is_null() {
        doc["x-pishoo"] = json!({});
    }
    if doc["x-pishoo"]["publisher"].is_null() {
        doc["x-pishoo"]["publisher"] = "alice.dhttp.net".into();
    }
    if doc["x-pishoo"]["name"].is_null() {
        doc["x-pishoo"]["name"] = "test".into();
    }
    if doc["info"]["description"].is_null() {
        doc["info"]["description"] = "Test fixture".into();
    }
    if doc["info"]["license"].is_null() {
        doc["info"]["license"] = json!({"name":"MIT"});
    }
    if doc["x-pishoo"]["host"].is_null() {
        doc["x-pishoo"]["host"] = json!({"private-storage":"Fixture storage","request-identity":"Fixture identity","signatures":"Fixture signatures"});
    }
    let mut ordinal = 0;
    for item in doc["paths"].as_object_mut().unwrap().values_mut() {
        for method in [
            "get", "head", "post", "put", "patch", "delete", "options", "trace",
        ] {
            if let Some(operation) = item.get_mut(method) {
                ordinal += 1;
                if operation["operationId"].is_null() {
                    operation["operationId"] = format!("fixture{ordinal}").into();
                }
                if operation["summary"].is_null() {
                    operation["summary"] = "Test operation".into();
                }
            }
        }
    }
    let signer = publisher(doc["x-pishoo"]["publisher"].as_str().unwrap(), 'a');
    package_fixture(&doc, &backend, Directory::default(), &signer)
}

pub(crate) fn frontend_document(version: &str) -> Value {
    json!({"openapi":"3.1.0","info":{"title":"Hello","version":version,"description":"Hello test","license":{"name":"MIT"}},
    "x-pishoo":{"publisher":"alice.dhttp.net","name":"hello","kind":"app","entry":"/","host":{}},
    "paths":{
        "/":{"get":{"operationId":"page","summary":"Page","x-pishoo":{"static-file":"/public/index.html"}},"head":{"operationId":"pageHead","summary":"Page headers","x-pishoo":{"static-file":"/public/index.html"}}},
        "/assets/app.js":{"get":{"operationId":"js","summary":"Script","x-pishoo":{"static-file":"/public/assets/app.js"}},"head":{"operationId":"jsHead","summary":"Script headers","x-pishoo":{"static-file":"/public/assets/app.js"}}},
        "/hello":{"post":{"operationId":"hello","summary":"Backend"}}
    }})
}

#[test]
fn flat_lib_snapshots_use_database_metadata_without_integrity_pins() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let runtime = WasmRuntime::new().unwrap();
    let mut document = frontend_document("1.0.0");
    document["x-pishoo"]["kind"] = json!("lib");
    document["x-pishoo"]
        .as_object_mut()
        .unwrap()
        .remove("entry");
    document["x-pishoo"]["host"] =
        json!({"private-storage":"Save messages", "request-identity":"Read identity"});
    document["paths"] = json!({"/hello":{"post":{"operationId":"hello","summary":"Hello"}}});
    let backend = include_bytes!("../../fixtures/wasi-http-read-request-then-respond.wasm");
    let bytes = package_fixture(
        &document,
        backend,
        Directory::default(),
        &publisher("alice.dhttp.net", 'a'),
    );
    let id = "alice.dhttp.net/hello";
    install_lib(&profile, id, &bytes, &runtime).unwrap();
    let path = profile.join("libs/hello.wasm");
    assert!(!profile.join("libs/hello").exists());
    let db = rusqlite::Connection::open(profile.config_db_path()).unwrap();
    let author: String = db
        .query_row("SELECT author FROM libs WHERE name='hello'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(author, "alice.dhttp.net");
    let columns = db
        .prepare("PRAGMA table_info(libs)")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        columns,
        [
            "name",
            "author",
            "version",
            "title",
            "description",
            "license",
            "repository"
        ]
    );
    let mut expected = package_metadata(&read_package(&bytes).unwrap()).unwrap();
    expected.as_object_mut().unwrap().remove("host");
    expected.as_object_mut().unwrap().remove("changelog");
    assert_eq!(installed_libs(&profile, Some(id)).unwrap(), expected);
    let lib_root = files::lib_root(&profile).unwrap();
    let snapshot = files::disk_bytes(&profile, &lib_root, id).unwrap();
    assert!(read_package(&snapshot).is_err());
    let snapshot =
        webc::Container::from_bytes_and_version(snapshot.into(), webc::Version::V3).unwrap();
    assert!(snapshot.get_volume("pishoo-signature").is_none());
    let original = std::fs::read(&path).unwrap();
    assert!(original.starts_with(backend));
    assert_eq!(snapshot.get_atom("backend").unwrap(), original);
    assert_eq!(
        package_file(&snapshot, "/metadata/openapi.json").unwrap(),
        package_file(&read_package(&bytes).unwrap(), "/metadata/openapi.json").unwrap()
    );
    let mut app_document = frontend_document("1.0.0");
    app_document["x-pishoo"]["dependencies"] =
        json!([{"publisher":"alice.dhttp.net","name":"hello","version":"1.0.0"}]);
    install_app(
        &profile,
        id,
        &package_fixture(
            &app_document,
            &[],
            frontend_files("dependent"),
            &publisher("alice.dhttp.net", 'a'),
        ),
    )
    .unwrap();
    let mut changed_document = document.clone();
    changed_document["info"]["description"] = "changed same version".into();
    let changed = package_fixture(
        &changed_document,
        backend,
        Directory::default(),
        &publisher("alice.dhttp.net", 'a'),
    );
    install_lib(&profile, id, &changed, &runtime).unwrap();
    assert_eq!(
        installed_libs(&profile, Some(id)).unwrap()["description"],
        "changed same version"
    );
    let replacement = package_fixture(
        &document,
        backend,
        Directory::default(),
        &publisher("alice.dhttp.net", 'b'),
    );
    let mut altered = original.clone();
    let position = original
        .windows(5)
        .position(|part| part == b"wasi:")
        .unwrap();
    altered[position] = b'v';
    std::fs::write(&path, &altered).unwrap();
    let snapshot = files::disk_bytes(&profile, &lib_root, id).unwrap();
    assert_eq!(
        webc::Container::from_bytes_and_version(snapshot.into(), webc::Version::V3)
            .unwrap()
            .get_atom("backend")
            .unwrap(),
        altered
    );
    install_lib(&profile, id, &replacement, &runtime).unwrap();
    install_lib(&profile, id, &bytes, &runtime).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), original);
    std::fs::remove_file(&path).unwrap();
    assert_eq!(
        installed_libs(&profile, Some(id)).unwrap()["version"],
        "1.0.0"
    );
    assert!(files::disk_bytes(&profile, &lib_root, id).is_err());
    install_lib(&profile, id, &replacement, &runtime).unwrap();
    install_lib(&profile, id, &bytes, &runtime).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), original);
    #[cfg(unix)]
    {
        let outside = root.path().join("outside.wasm");
        std::fs::write(&outside, &original).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        assert!(files::disk_bytes(&profile, &lib_root, id).is_err());
        assert_eq!(installed_libs(&profile, Some(id)).unwrap()["id"], id);
        assert_eq!(std::fs::read(&outside).unwrap(), original);
    }
}

#[test]
#[ignore = "requires root; run in an isolated filesystem"]
fn root_package_installs_and_updates_belong_to_identity_owner() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    assert!(rustix::process::geteuid().is_root());
    let root = tempfile::tempdir().unwrap();
    let runtime = WasmRuntime::new().unwrap();
    for name in ["alice.dhttp.net", "std.dhttp.net"] {
        let profile = IdentityProfile::try_from(root.path().join(name)).unwrap();
        std::fs::create_dir_all(profile.path()).unwrap();
        let identity = std::fs::File::open(profile.path()).unwrap();
        rustix::fs::fchown(
            &identity,
            Some(rustix::fs::Uid::from_raw(10001)),
            Some(rustix::fs::Gid::from_raw(10002)),
        )
        .unwrap();
        crate::setup::load_server_config(&profile).unwrap();
        for version in ["1.0.0", "2.0.0"] {
            let mut document = frontend_document(version);
            document["x-pishoo"]["publisher"] = json!(name);
            let app = package_fixture(
                &document,
                &[],
                Directory::new(
                    BTreeMap::from([("index.html".parse().unwrap(), b"page".to_vec().into())]),
                    Timestamps::default(),
                ),
                &publisher(name, 'a'),
            );
            install_app(&profile, &format!("{name}/hello"), &app).unwrap();
            document["x-pishoo"]["kind"] = json!("lib");
            document["x-pishoo"]
                .as_object_mut()
                .unwrap()
                .remove("entry");
            document["paths"] =
                json!({"/hello":{"post":{"operationId":"hello","summary":"Hello"}}});
            let lib = package_fixture(
                &document,
                include_bytes!("../../fixtures/wasi-http-read-request-then-respond.wasm"),
                Directory::default(),
                &publisher(name, 'a'),
            );
            install_lib(&profile, &format!("{name}/hello"), &lib, &runtime).unwrap();
            for kind in ["apps", "libs"] {
                let mut pending = vec![profile.join(kind)];
                while let Some(path) = pending.pop() {
                    let metadata = path.metadata().unwrap();
                    assert_eq!(
                        (metadata.uid(), metadata.gid()),
                        (10001, 10002),
                        "{}",
                        path.display()
                    );
                    assert_eq!(
                        metadata.permissions().mode() & 0o777,
                        if metadata.is_dir() { 0o700 } else { 0o600 }
                    );
                    if metadata.is_dir() {
                        pending.extend(
                            std::fs::read_dir(path)
                                .unwrap()
                                .map(|entry| entry.unwrap().path()),
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn expanded_app_loads_installed_public_files_without_revalidation_and_rejects_symlinks() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.join("libs")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let id = "alice.dhttp.net/hello";
    install_app(&profile, id, &frontend_package("1.0.0", "verified page")).unwrap();
    let directory = profile.join("apps/hello");
    assert!(!directory.join("app.webc").exists());
    let index = directory.join("public/index.html");
    assert_eq!(std::fs::read(&index).unwrap(), b"verified page");
    std::fs::write(&index, b"modified page").unwrap();
    let apps = installed_apps(&profile, Some(id)).unwrap();
    assert_eq!(
        package_file(&apps[id], "/public/index.html").unwrap(),
        &b"modified page"[..]
    );
    std::fs::write(&index, b"verified page").unwrap();
    assert!(installed_apps(&profile, Some(id)).is_ok());
    #[cfg(unix)]
    {
        let outside = root.path().join("outside-assets");
        let assets = directory.join("public/assets");
        std::fs::rename(&assets, &outside).unwrap();
        std::os::unix::fs::symlink(&outside, &assets).unwrap();
        assert!(installed_apps(&profile, Some(id)).is_err());
        assert!(outside.join("app.js").is_file());
    }
}

#[test]
fn http_installed_catalog_uses_database_metadata_without_opening_payloads() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.path()).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let id = "alice.dhttp.net/hello";
    install_app(&profile, id, &frontend_package("1.0.0", "installed")).unwrap();
    let mut sandbox = Sandbox::new(Arc::new(WasmRuntime::new().unwrap()));
    sandbox.apps = installed_apps(&profile, None).unwrap();
    // The HTTP catalog must not inspect any saved payload, even when none of
    // those files are readable. Installation facts still come from config.db.
    std::fs::remove_dir_all(profile.join("apps")).unwrap();
    let value = installed_catalog(&profile, true, Some("hello")).unwrap();
    assert_eq!(value["id"], id);
    assert_eq!(value["version"], "1.0.0");
    assert_eq!(value["publisher"], "alice.dhttp.net");
    assert_eq!(value["entry"], "/");
    assert!(value.get("host").is_none());
    assert_eq!(
        value["title"],
        lib_metadata(&sandbox.apps[id], Some(id))["title"]
    );
    let db = rusqlite::Connection::open(profile.config_db_path()).unwrap();
    db.execute(
        "INSERT INTO libs(name,author,version) VALUES('backend','alice.dhttp.net','2.0.0')",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO app_dependencies VALUES(?1,'backend','2.0.0')",
        [id.rsplit('/').next().unwrap()],
    )
    .unwrap();
    db.execute(
        "UPDATE apps SET version='2.0.0',title='Database title',description='Database description' WHERE name=?1",
        [id.rsplit('/').next().unwrap()],
    ).unwrap();
    let value = installed_catalog(&profile, true, Some("hello")).unwrap();
    assert_eq!(value["version"], "2.0.0");
    assert_eq!(value["title"], "Database title");
    assert_eq!(value["description"], "Database description");
    assert_eq!(
        value["dependencies"],
        json!([{"publisher":"alice.dhttp.net","name":"backend","version":"2.0.0"}])
    );
    let value = installed_catalog(&profile, false, None).unwrap();
    assert_eq!(value[0]["id"], "alice.dhttp.net/backend");
    assert_eq!(value[0]["version"], "2.0.0");
    assert_eq!(value[0]["title"], "backend");
    assert_eq!(value[0]["description"], Value::Null);
    assert!(value[0].get("host").is_none());
    assert!(value[0].get("repository").is_none());
    db.execute("UPDATE libs SET title='Backend' WHERE name='backend'", [])
        .unwrap();
    let value = installed_catalog(&profile, false, Some("backend")).unwrap();
    assert_eq!(value["title"], "Backend");
    assert_eq!(value["publisher"], "alice.dhttp.net");
    assert!(value.get("host").is_none());
    assert!(matches!(
        installed_catalog(&profile, false, Some("missing")),
        Err(Error::RouteNotFound)
    ));
    db.execute(
        "DELETE FROM apps WHERE name=?1",
        [id.rsplit('/').next().unwrap()],
    )
    .unwrap();
    assert!(
        installed_catalog(&profile, true, None)
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(sandbox.apps.contains_key(id));

    // A pre-metadata schema is still queryable without reading any payload.
    let legacy = IdentityProfile::try_from(root.path().join("legacy")).unwrap();
    std::fs::create_dir_all(legacy.join("db")).unwrap();
    let old_db = rusqlite::Connection::open(legacy.config_db_path()).unwrap();
    old_db
        .execute_batch(
            "CREATE TABLE libs(id TEXT PRIMARY KEY,name TEXT,version TEXT); \
         CREATE TABLE lib_dependencies(lib_id TEXT,dependency_id TEXT,version TEXT); \
         INSERT INTO libs VALUES('alice.dhttp.net/old','old','1.0.0');",
        )
        .unwrap();
    let value = installed_catalog(&legacy, false, Some("old")).unwrap();
    assert_eq!(value["title"], "old");
    assert_eq!(value["description"], Value::Null);
    assert!(value.get("host").is_none());
    assert_eq!(value["dependencies"], json!([]));
}

#[test]
fn signed_std_publisher_selects_standard_libs_but_apps_share_one_directory() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.join("libs")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let authority = publisher("std.dhttp.net", 'a');
    let mut lib = frontend_document("1.0.0");
    lib["x-pishoo"]["publisher"] = json!("std.dhttp.net");
    lib["x-pishoo"]["kind"] = json!("lib");
    lib["x-pishoo"].as_object_mut().unwrap().remove("entry");
    lib["paths"] = json!({"/hello":{"post":{"operationId":"hello","summary":"Hello"}}});
    let bytes = package_fixture(
        &lib,
        include_bytes!("../../fixtures/wasi-http-read-request-then-respond.wasm"),
        Directory::default(),
        &authority,
    );
    let id = "std.dhttp.net/hello";
    install_lib(&profile, id, &bytes, &runtime).unwrap();
    let mut app = frontend_document("1.0.0");
    app["x-pishoo"]["publisher"] = json!("std.dhttp.net");
    let frontend = package_fixture(&app, &[], frontend_files("standard page"), &authority);
    install_app(&profile, id, &frontend).unwrap();
    assert!(profile.join("libs/std/hello.wasm").is_file());
    assert!(profile.join("apps/hello/public/index.html").is_file());
    assert!(!profile.join("libs/hello").exists());
    assert!(!profile.join("apps/std").exists());
    let mut sandbox = Sandbox::new(runtime.clone());
    sandbox.load_libs(&profile).unwrap();
    assert!(sandbox.libs.contains_key(id));
    assert!(sandbox.apps.contains_key(id));
    install_lib(&profile, id, &bytes, &runtime).unwrap();
    assert!(profile.join("libs/std/hello.wasm").is_file());
    assert!(!profile.join("libs/hello").exists());
    remove_app(&profile, id).unwrap();
    remove_lib(&profile, id).unwrap();
    assert!(!profile.join("apps/hello").exists());
    assert!(!profile.join("libs/std/hello.wasm").exists());

    // Familiar standard names and a forged install ID do not classify other signers.
    let ordinary = publisher("alice.dhttp.net", 'a');
    lib["x-pishoo"]["publisher"] = json!("alice.dhttp.net");
    lib["x-pishoo"]["name"] = json!("message");
    let bytes = package_fixture(
        &lib,
        include_bytes!("../../fixtures/wasi-http-read-request-then-respond.wasm"),
        Directory::default(),
        &ordinary,
    );
    assert!(install_lib(&profile, "std.dhttp.net/message", &bytes, &runtime).is_err());
    install_lib(&profile, "alice.dhttp.net/message", &bytes, &runtime).unwrap();
    assert!(profile.join("libs/message.wasm").is_file());
    assert!(!profile.join("libs/std/message").exists());
}

fn legacy_schema(profile: &IdentityProfile) -> rusqlite::Connection {
    let db = rusqlite::Connection::open(profile.config_db_path()).unwrap();
    db.execute_batch("PRAGMA foreign_keys=OFF; DROP TABLE lib_capability_operations; DROP TABLE lib_capabilities; DROP TABLE app_dependencies; DROP TABLE lib_dependencies; DROP TABLE libs; DROP TABLE apps;
        CREATE TABLE libs(id TEXT PRIMARY KEY NOT NULL,name TEXT UNIQUE NOT NULL,version TEXT NOT NULL);
        CREATE TABLE apps(id TEXT PRIMARY KEY NOT NULL,name TEXT UNIQUE NOT NULL,version TEXT NOT NULL);
        CREATE TABLE app_dependencies(app_id TEXT REFERENCES apps(id) ON DELETE CASCADE,lib_id TEXT REFERENCES libs(id) ON DELETE RESTRICT,version TEXT,PRIMARY KEY(app_id,lib_id));
        CREATE TABLE lib_dependencies(lib_id TEXT REFERENCES libs(id) ON DELETE CASCADE,dependency_id TEXT REFERENCES libs(id) ON DELETE RESTRICT,version TEXT,PRIMARY KEY(lib_id,dependency_id));
        CREATE TABLE lib_capabilities(lib_id TEXT REFERENCES libs(id) ON DELETE CASCADE,capability_id TEXT,title TEXT,PRIMARY KEY(lib_id,capability_id));
        CREATE TABLE lib_capability_operations(lib_id TEXT,capability_id TEXT,method TEXT,path TEXT,PRIMARY KEY(lib_id,capability_id,method,path),FOREIGN KEY(lib_id,capability_id) REFERENCES lib_capabilities(lib_id,capability_id) ON DELETE CASCADE); PRAGMA foreign_keys=ON;").unwrap();
    db
}

#[test]
fn registered_legacy_webc_is_preserved_and_expands_on_explicit_reinstall() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    std::fs::create_dir_all(profile.join("libs")).unwrap();
    let db = legacy_schema(&profile);
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let mut document = frontend_document("1.0.0");
    document["x-pishoo"]["publisher"] = json!("std.dhttp.net");
    document["x-pishoo"]["kind"] = json!("lib");
    document["x-pishoo"]
        .as_object_mut()
        .unwrap()
        .remove("entry");
    document["paths"] = json!({"/hello":{"post":{"operationId":"hello","summary":"Hello"}}});
    let bytes = package_fixture(
        &document,
        include_bytes!("../../fixtures/wasi-http-read-request-then-respond.wasm"),
        Directory::default(),
        &publisher("std.dhttp.net", 'a'),
    );
    let id = "std.dhttp.net/hello";
    db.execute("INSERT INTO libs VALUES(?1,'hello','1.0.0')", [id])
        .unwrap();
    let legacy = profile.join("lib/std.dhttp.net/hello/lib.webc");
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    std::fs::write(&legacy, &bytes).unwrap();
    let mut sandbox = Sandbox::new(runtime.clone());
    sandbox.load_libs(&profile).unwrap();
    assert!(sandbox.libs.contains_key(id));
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM pragma_table_info('libs') WHERE name='id'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    assert!(!profile.join("libs/std/hello.wasm").exists());
    let mut corrupted = bytes.clone();
    let position = corrupted
        .windows(5)
        .position(|part| part == b"wasi:")
        .unwrap();
    corrupted[position] = b'v';
    std::fs::write(&legacy, &corrupted).unwrap();
    let replacement = package_fixture(
        &document,
        include_bytes!("../../fixtures/wasi-http-read-request-then-respond.wasm"),
        Directory::default(),
        &publisher("std.dhttp.net", 'b'),
    );
    install_lib(&profile, id, &replacement, &runtime).unwrap();
    install_lib(&profile, id, &bytes, &runtime).unwrap();
    assert!(profile.join("libs/std/hello.wasm").is_file());
    assert_eq!(
        db.query_row("SELECT author FROM libs WHERE name='hello'", [], |row| {
            row.get::<_, String>(0)
        })
        .unwrap(),
        "std.dhttp.net"
    );
    assert_eq!(std::fs::read(&legacy).unwrap(), corrupted);
}

#[test]
fn legacy_expanded_proof_is_read_without_startup_migration() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    std::fs::create_dir_all(profile.join("libs")).unwrap();
    let db = legacy_schema(&profile);
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let mut document = frontend_document("1.0.0");
    document["x-pishoo"]["kind"] = json!("lib");
    document["x-pishoo"]
        .as_object_mut()
        .unwrap()
        .remove("entry");
    document["paths"] = json!({"/hello":{"post":{"operationId":"hello","summary":"Hello"}}});
    let backend = include_bytes!("../../fixtures/wasi-http-read-request-then-respond.wasm");
    let bytes = package_fixture(
        &document,
        backend,
        Directory::default(),
        &publisher("alice.dhttp.net", 'a'),
    );
    let id = "alice.dhttp.net/hello";
    db.execute("INSERT INTO libs VALUES(?1,'hello','1.0.0')", [id])
        .unwrap();
    let reader = webc::v3::read::OwnedReader::parse(bytes.clone()).unwrap();
    let atom = &reader.index().atoms;
    let section = webc::v3::read::Section::parse(
        3,
        Some(
            bytes[atom.span.start + 1..atom.span.start + 33]
                .try_into()
                .unwrap(),
        ),
        bytes[atom.span.start + 41..atom.span.end()].to_vec().into(),
    )
    .unwrap();
    let (_, _, _, span) = section
        .as_atoms()
        .unwrap()
        .iter_with_offsets()
        .next()
        .unwrap()
        .unwrap();
    let span = span.with_offset(atom.span.start);
    let header = serde_json::to_vec(&(bytes.len(), vec![("hello.wasm", span)])).unwrap();
    let mut proof = b"pishoo-expanded-webc-v1\0".to_vec();
    proof.extend_from_slice(&(header.len() as u64).to_le_bytes());
    proof.extend_from_slice(&header);
    proof.extend_from_slice(&bytes[..span.start]);
    proof.extend_from_slice(&bytes[span.end()..]);
    let old = profile.join("libs/hello");
    std::fs::create_dir(&old).unwrap();
    std::fs::write(old.join("hello.wasm"), backend).unwrap();
    std::fs::write(old.join(".webc-proof"), &proof).unwrap();
    let mut sandbox = Sandbox::new(runtime.clone());
    sandbox.load_libs(&profile).unwrap();
    assert!(sandbox.libs.contains_key(id));
    assert!(!profile.join("libs/hello.wasm").exists());
    assert_eq!(
        files::disk_bytes(&profile, &files::lib_root(&profile).unwrap(), id).unwrap(),
        bytes
    );
    install_lib(&profile, id, &bytes, &runtime).unwrap();
    assert!(profile.join("libs/hello.wasm").is_file());
    assert_eq!(std::fs::read(old.join(".webc-proof")).unwrap(), proof);
}

#[test]
fn explicit_management_migrates_natural_keys_and_preserves_relations_atomically() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let db = legacy_schema(&profile);
    db.execute_batch("INSERT INTO libs VALUES('alice.dhttp.net/first','first','1.0.0'),('alice.dhttp.net/parent','parent','1.0.0'),('legacy','legacy','1.0.0'); INSERT INTO apps VALUES('alice.dhttp.net/front','front','1.0.0'); INSERT INTO app_dependencies VALUES('alice.dhttp.net/front','alice.dhttp.net/parent','1.0.0'); INSERT INTO lib_dependencies VALUES('alice.dhttp.net/parent','alice.dhttp.net/first','1.0.0'); INSERT INTO lib_capabilities VALUES('alice.dhttp.net/parent','message','Message'); INSERT INTO lib_capability_operations VALUES('alice.dhttp.net/parent','message','POST','/std/message');").unwrap();
    assert_eq!(
        installed_libs(&profile, Some("legacy")).unwrap()["id"],
        "legacy"
    );
    remove_lib(&profile, "absent").unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM pragma_table_info('libs')",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        7
    );
    assert_eq!(
        db.query_row("SELECT author FROM libs WHERE name='first'", [], |row| {
            row.get::<_, String>(0)
        })
        .unwrap(),
        "alice.dhttp.net"
    );
    assert!(
        db.query_row("SELECT author FROM libs WHERE name='legacy'", [], |row| row
            .get::<_, Option<String>>(0))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        db.query_row(
            "SELECT app_name,lib_name FROM app_dependencies",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        )
        .unwrap(),
        ("front".into(), "parent".into())
    );
    assert_eq!(
        db.query_row(
            "SELECT lib_name,dependency_name FROM lib_dependencies",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        )
        .unwrap(),
        ("parent".into(), "first".into())
    );
    assert_eq!(
        db.query_row("SELECT lib_name FROM lib_capabilities", [], |row| row
            .get::<_, String>(0))
            .unwrap(),
        "parent"
    );
    assert!(
        !db.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_foreign_key_check)",
            [],
            |row| row.get::<_, bool>(0)
        )
        .unwrap()
    );
    let broken = legacy_schema(&profile);
    broken.execute_batch("PRAGMA foreign_keys=OFF; INSERT INTO libs VALUES('alice.dhttp.net/parent','parent','1.0.0'); INSERT INTO lib_dependencies VALUES('alice.dhttp.net/parent','missing','1.0.0');").unwrap();
    assert!(remove_lib(&profile, "absent").is_err());
    assert_eq!(
        broken
            .query_row(
                "SELECT count(*) FROM pragma_table_info('libs') WHERE name='id'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    assert_eq!(
        broken
            .query_row("SELECT dependency_id FROM lib_dependencies", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
        "missing"
    );
}

pub(crate) fn frontend_files(text: &'static str) -> Directory<'static> {
    Directory::new(
        BTreeMap::from([
            (
                "index.html".parse().unwrap(),
                text.as_bytes().to_vec().into(),
            ),
            (
                "assets".parse().unwrap(),
                webc::v3::write::DirEntry::Dir(Directory::new(
                    BTreeMap::from([(
                        "app.js".parse().unwrap(),
                        b"fetch('./hello')".to_vec().into(),
                    )]),
                    Timestamps::default(),
                )),
            ),
        ]),
        Timestamps::default(),
    )
}
fn frontend_package(version: &str, text: &'static str) -> Vec<u8> {
    package_fixture(
        &frontend_document(version),
        include_bytes!("../../fixtures/wasi-http-read-request-then-respond.wasm"),
        frontend_files(text),
        &publisher("alice.dhttp.net", 'a'),
    )
}

#[test]
fn package_routes_files_and_identity_are_bound_to_publisher_signature() {
    let bytes = frontend_package("1.0.0", "<h1>Old frontend</h1>");
    let package = read_package(&bytes).unwrap();
    assert_eq!(
        package_file(&package, "/public/assets/app.js").unwrap(),
        "fetch('./hello')"
    );
    for path in [
        "/public/../metadata/openapi.json",
        "/public//index.html",
        "/public/%2e%2e/secret",
        "/metadata/secret",
        "/ssl/privkey.pem",
    ] {
        assert!(package_file(&package, path).is_err(), "{path}");
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(read_package(&trailing).is_err());
    let mut changed = bytes.clone();
    let position = changed
        .windows(b"Old frontend".len())
        .position(|s| s == b"Old frontend")
        .unwrap();
    changed[position] = b'X';
    assert!(read_package(&changed).is_err());
    // App routes are derived from files and do not use any API declaration.
    let mut bad = frontend_document("1.0.0");
    bad["x-pishoo"]["entry"] = "/other".into();
    assert!(
        read_package(&package_fixture(
            &bad,
            &[],
            frontend_files("page"),
            &publisher("alice.dhttp.net", 'a')
        ))
        .is_err()
    );
    assert!(package.atoms().is_empty());
    assert!(package_file(&package, "/metadata/openapi.json").is_err());
    let wrong = package_fixture(
        &frontend_document("1.0.0"),
        include_bytes!("../../fixtures/wasi-http-read-request-then-respond.wasm"),
        frontend_files("page"),
        &publisher("bob.dhttp.net", 'a'),
    );
    assert!(read_package(&wrong).is_err());

    // Recompute all WEBC checksums after changing frontend bytes, keeping the
    // original envelope: internal integrity must not substitute for a signature.
    let forged = webc::Container::from_bytes_and_version(
        frontend_package("1.0.0", "<h1>Attacker frontend</h1>").into(),
        webc::Version::V3,
    )
    .unwrap();
    let mut writer = Writer::default()
        .write_manifest(forged.manifest())
        .unwrap()
        .write_atoms(
            forged
                .atoms()
                .into_iter()
                .map(|(name, data)| {
                    (
                        name.parse().unwrap(),
                        FileEntry::owned(data.to_vec(), Timestamps::default()),
                    )
                })
                .collect(),
        )
        .unwrap();
    for (name, volume) in forged.volumes() {
        let volume = if name == "pishoo-signature" {
            package.get_volume(&name).unwrap()
        } else {
            volume
        };
        writer
            .write_volume(&name, copy_directory(&volume, "/"))
            .unwrap();
    }
    let forged = writer.finish(SignatureAlgorithm::None).unwrap();
    assert!(
        webc::Container::from_bytes_and_version(forged.clone(), webc::Version::V3)
            .unwrap()
            .validate()
            .is_ok()
    );
    assert!(read_package(&forged).is_err());
}

#[tokio::test]
async fn frontend_updates_hold_package_snapshots_and_static_requests_skip_guest() {
    use http::{Method, StatusCode};
    use tower::ServiceExt;
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.join("apps")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let id = "alice.dhttp.net/hello";
    install_app(&profile, id, &frontend_package("1.0.0", "old page")).unwrap();
    let mut sandbox = Sandbox::new(runtime.clone());
    sandbox.load_libs(&profile).unwrap();
    let old = api_router(&sandbox, crate::test_identity::endpoint("alice.dhttp.net"));
    install_app(&profile, id, &frontend_package("2.0.0", "new page")).unwrap();
    let before = std::fs::read(
        profile
            .join("apps")
            .join(id.rsplit('/').next().unwrap())
            .join("public/index.html"),
    )
    .unwrap();
    assert!(install_app(&profile, id, b"broken").is_err());
    assert_eq!(
        std::fs::read(
            profile
                .join("apps")
                .join(id.rsplit('/').next().unwrap())
                .join("public/index.html")
        )
        .unwrap(),
        before
    );
    sandbox.load_libs(&profile).unwrap();
    let new = api_router(&sandbox, crate::test_identity::endpoint("alice.dhttp.net"));
    for (router, expected) in [(&old, "old page"), (&new, "new page")] {
        for method in [Method::GET, Method::HEAD] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method.clone())
                        .uri(format!("/{}/?view=1", id.rsplit('/').next().unwrap()))
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()["content-type"], "text/html");
            assert_eq!(
                response.headers()["content-length"],
                expected.len().to_string()
            );
            let body = response.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(
                body.as_ref(),
                if method == Method::HEAD {
                    b""
                } else {
                    expected.as_bytes()
                }
            );
        }
    }
    assert!(sandbox.tasks.is_empty());
    let response = new
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/{}?value=1", id.rsplit('/').next().unwrap()))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        response.headers()["location"],
        format!("/{}/?value=1", id.rsplit('/').next().unwrap())
    );
    for (method, path, status) in [
        (Method::POST, "/", StatusCode::METHOD_NOT_ALLOWED),
        (Method::GET, "/index.html", StatusCode::OK),
        (Method::GET, "/contacts/deep-link", StatusCode::OK),
        (Method::HEAD, "/contacts/deep-link", StatusCode::OK),
        (Method::GET, "/missing", StatusCode::OK),
        (Method::GET, "/hello", StatusCode::OK),
        (Method::POST, "/hello", StatusCode::METHOD_NOT_ALLOWED),
        (Method::GET, "/missing.js", StatusCode::NOT_FOUND),
        (Method::GET, "/assets/missing", StatusCode::NOT_FOUND),
        (Method::GET, "/assets/app.js", StatusCode::OK),
    ] {
        let response = new
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(format!("/{}{path}", id.rsplit('/').next().unwrap()))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), status, "{path}");
    }
    remove_app(&profile, id).unwrap();
    assert!(!profile.join("db").join(id).exists());
    // Running router owns the old package even after the disk resource is removed.
    let response = old
        .oneshot(
            Request::builder()
                .uri(format!("/{}/", id.rsplit('/').next().unwrap()))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "old page"
    );
}

#[tokio::test]
async fn independent_app_install_mount_update_remove_and_dependency_protection() {
    use http::{Method, StatusCode};
    use tower::ServiceExt;
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.join("libs")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let authority = publisher("alice.dhttp.net", 'a');
    let mut backend = frontend_document("1.0.0");
    backend["x-pishoo"]["kind"] = json!("lib");
    backend["x-pishoo"].as_object_mut().unwrap().remove("entry");
    backend["paths"]
        .as_object_mut()
        .unwrap()
        .retain(|path, _| path == "/hello");
    backend["paths"]["/hello/run"] = backend["paths"]["/hello"].clone();
    backend["paths"].as_object_mut().unwrap().remove("/hello");
    let lib_bytes = package_fixture(
        &backend,
        include_bytes!("../../fixtures/wasi-http-read-request-then-respond.wasm"),
        Directory::default(),
        &authority,
    );
    let mut frontend = frontend_document("1.0.0");
    frontend["x-pishoo"]["kind"] = json!("app");
    frontend["x-pishoo"]["dependencies"] =
        json!([{"publisher":"alice.dhttp.net","name":"hello","version":"1.0.0"}]);
    frontend["paths"].as_object_mut().unwrap().remove("/hello");
    let app_bytes = package_fixture(&frontend, &[], frontend_files("separate page"), &authority);
    assert!(read_package(&app_bytes).unwrap().atoms().is_empty());
    assert!(check_lib(&app_bytes, &runtime).is_err());
    assert!(install_lib(&profile, "alice.dhttp.net/hello", &app_bytes, &runtime).is_err());
    assert!(install_app(&profile, "alice.dhttp.net/hello", &app_bytes).is_err());
    install_lib(&profile, "alice.dhttp.net/hello", &lib_bytes, &runtime).unwrap();
    let metadata = install_app(&profile, "alice.dhttp.net/hello", &app_bytes).unwrap();
    assert_eq!(metadata["kind"], "app");
    assert!(
        metadata["endpoints"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["path"].as_str().unwrap().starts_with("/app/"))
    );
    assert!(profile.join("apps/hello/public/index.html").is_file());
    assert!(!profile.join("apps/hello/manifest.json").exists());
    assert!(!profile.join("apps/hello/signature.json").exists());
    assert!(!profile.join("apps/hello/.webc-proof").exists());
    assert!(remove_lib(&profile, "alice.dhttp.net/hello").is_err());
    backend["info"]["version"] = json!("2.0.0");
    let changed = package_fixture(
        &backend,
        include_bytes!("../../fixtures/wasi-http-read-request-then-respond.wasm"),
        Directory::default(),
        &authority,
    );
    assert!(install_lib(&profile, "alice.dhttp.net/hello", &changed, &runtime).is_err());
    let mut sandbox = Sandbox::new(runtime);
    sandbox.load_libs(&profile).unwrap();
    assert_eq!(sandbox.libs.len(), 1);
    assert_eq!(sandbox.apps.len(), 1);
    let snapshot = api_router(&sandbox, crate::test_identity::endpoint("alice.dhttp.net"));
    for method in [Method::GET, Method::HEAD] {
        let response = snapshot
            .clone()
            .oneshot(
                Request::builder()
                    .method(method.clone())
                    .uri("/hello/")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-length"], "13");
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            bytes.as_ref(),
            if method == Method::HEAD {
                b""
            } else {
                b"separate page".as_slice()
            }
        );
    }
    let response = snapshot
        .clone()
        .oneshot(
            Request::builder()
                .uri("/hello?view=1")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(response.headers()["location"], "/hello/?view=1");
    let response = snapshot
        .clone()
        .oneshot(
            Request::builder()
                .uri("/hello/contacts/deep-link")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = snapshot
        .clone()
        .oneshot(
            Request::builder()
                .uri("/hello/assets/missing.js")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let old = std::fs::read(profile.join("apps/hello/public/index.html")).unwrap();
    assert!(install_app(&profile, "alice.dhttp.net/hello", b"broken").is_err());
    assert_eq!(
        std::fs::read(profile.join("apps/hello/public/index.html")).unwrap(),
        old
    );
    let db = rusqlite::Connection::open(profile.config_db_path()).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT lib_name,version FROM app_dependencies WHERE app_name='hello'",
            [],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        )
        .unwrap(),
        ("hello".into(), "1.0.0".into())
    );
    std::fs::write(profile.join("db/hello/saved.db"), b"preserve backend data").unwrap();
    remove_app(&profile, "alice.dhttp.net/hello").unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM apps", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM app_dependencies", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM libs", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        std::fs::read(profile.join("db/hello/saved.db")).unwrap(),
        b"preserve backend data"
    );
    assert!(profile.join("libs/hello.wasm").is_file());
    sandbox.load_libs(&profile).unwrap();
    assert!(sandbox.apps.is_empty());
    assert_eq!(sandbox.libs.len(), 1);
    remove_lib(&profile, "alice.dhttp.net/hello").unwrap();
    assert_eq!(
        std::fs::read(profile.join("db/hello/saved.db")).unwrap(),
        b"preserve backend data"
    );
}

#[test]
fn names_are_unique_per_kind_and_registration_updates_are_atomic() {
    let directory = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(directory.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.join("libs")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let runtime = WasmRuntime::new().unwrap();
    let fixture = |name: &str, kind: &str, version: &str| {
        let mut doc = frontend_document(version);
        doc["x-pishoo"]["publisher"] = json!(name);
        doc["x-pishoo"]["kind"] = json!(kind);
        if kind == "app" {
            doc["x-pishoo"]["dependencies"] = json!([]);
            doc["paths"].as_object_mut().unwrap().remove("/hello");
            package_fixture(
                &doc,
                &[],
                frontend_files("page"),
                &publisher(name, if name.starts_with("alice") { 'a' } else { 'b' }),
            )
        } else {
            doc["x-pishoo"].as_object_mut().unwrap().remove("entry");
            doc["paths"]
                .as_object_mut()
                .unwrap()
                .retain(|path, _| path == "/hello");
            package_fixture(
                &doc,
                include_bytes!("../../fixtures/wasi-http-read-request-then-respond.wasm"),
                Directory::default(),
                &publisher(name, if name.starts_with("alice") { 'a' } else { 'b' }),
            )
        }
    };
    let lib = fixture("alice.dhttp.net", "lib", "1.0.0");
    let app = fixture("bob.dhttp.net", "app", "1.0.0");
    install_lib(&profile, "alice.dhttp.net/hello", &lib, &runtime).unwrap();
    install_app(&profile, "bob.dhttp.net/hello", &app).unwrap();
    assert!(
        install_lib(
            &profile,
            "bob.dhttp.net/hello",
            &fixture("bob.dhttp.net", "lib", "1.0.0"),
            &runtime
        )
        .unwrap_err()
        .to_string()
        .contains("already installed")
    );
    assert!(
        install_app(
            &profile,
            "alice.dhttp.net/hello",
            &fixture("alice.dhttp.net", "app", "1.0.0")
        )
        .unwrap_err()
        .to_string()
        .contains("already installed")
    );
    assert!(install_lib(&profile, "hello", &lib, &runtime).is_err());
    let saved_snapshot = files::disk_bytes(
        &profile,
        &files::lib_root(&profile).unwrap(),
        "alice.dhttp.net/hello",
    )
    .unwrap();
    let db = rusqlite::Connection::open(profile.config_db_path()).unwrap();
    db.execute_batch("CREATE TRIGGER reject_update BEFORE UPDATE ON libs BEGIN SELECT RAISE(ABORT,'injected failure'); END;").unwrap();
    assert!(
        install_lib(
            &profile,
            "alice.dhttp.net/hello",
            &fixture("alice.dhttp.net", "lib", "2.0.0"),
            &runtime
        )
        .is_err()
    );
    assert_eq!(
        files::disk_bytes(
            &profile,
            &files::lib_root(&profile).unwrap(),
            "alice.dhttp.net/hello"
        )
        .unwrap(),
        saved_snapshot
    );
    assert_eq!(
        db.query_row("SELECT version FROM libs", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "1.0.0"
    );
    db.execute_batch("DROP TRIGGER reject_update;").unwrap();
    db.execute_batch("BEGIN;").unwrap();
    db.query_row("SELECT count(*) FROM libs", [], |r| r.get::<_, i64>(0))
        .unwrap();
    assert!(
        install_lib(
            &profile,
            "alice.dhttp.net/hello",
            &fixture("alice.dhttp.net", "lib", "2.0.0"),
            &runtime
        )
        .is_err()
    );
    db.execute_batch("ROLLBACK;").unwrap();
    assert_eq!(
        files::disk_bytes(
            &profile,
            &files::lib_root(&profile).unwrap(),
            "alice.dhttp.net/hello"
        )
        .unwrap(),
        saved_snapshot
    );
    assert_eq!(
        db.query_row("SELECT version FROM libs", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "1.0.0"
    );

    install_lib(
        &profile,
        "alice.dhttp.net/hello",
        &fixture("alice.dhttp.net", "lib", "2.0.0"),
        &runtime,
    )
    .unwrap();
    assert_eq!(
        db.query_row("SELECT version FROM libs", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "2.0.0"
    );
    let saved_app_manifest = serde_json::to_value(
        installed_apps(&profile, Some("bob.dhttp.net/hello")).unwrap()["bob.dhttp.net/hello"]
            .manifest(),
    )
    .unwrap();
    db.execute_batch("CREATE TRIGGER reject_remove BEFORE DELETE ON apps BEGIN SELECT RAISE(ABORT,'injected failure'); END;").unwrap();
    assert!(remove_app(&profile, "bob.dhttp.net/hello").is_err());
    assert_eq!(
        serde_json::to_value(
            installed_apps(&profile, Some("bob.dhttp.net/hello")).unwrap()["bob.dhttp.net/hello"]
                .manifest()
        )
        .unwrap(),
        saved_app_manifest
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM apps", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    db.execute_batch("DROP TRIGGER reject_remove;").unwrap();
    remove_lib(&profile, "alice.dhttp.net/hello").unwrap();
    let alice = fixture("alice.dhttp.net", "lib", "1.0.0");
    let bob = fixture("bob.dhttp.net", "lib", "1.0.0");
    let barrier = std::sync::Barrier::new(2);
    let results = std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            barrier.wait();
            install_lib(&profile, "alice.dhttp.net/hello", &alice, &runtime)
        });
        let second = scope.spawn(|| {
            barrier.wait();
            install_lib(&profile, "bob.dhttp.net/hello", &bob, &runtime)
        });
        [first.join().unwrap(), second.join().unwrap()]
    });
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        db.query_row("SELECT count(*) FROM libs WHERE name='hello'", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn backend_dependencies_are_multiple_pinned_persistent_and_acyclic() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.join("libs")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let authority = publisher("alice.dhttp.net", 'a');
    let package = |name: &str, version: &str, dependencies: Value| {
        let mut doc = frontend_document(version);
        doc["x-pishoo"]["kind"] = json!("lib");
        doc["x-pishoo"]["name"] = json!(name);
        doc["x-pishoo"]["dependencies"] = dependencies;
        doc["paths"][format!("/{name}")] = doc["paths"]["/hello"].clone();
        doc["paths"].as_object_mut().unwrap().remove("/hello");
        doc["x-pishoo"].as_object_mut().unwrap().remove("entry");
        doc["paths"]
            .as_object_mut()
            .unwrap()
            .retain(|path, _| path == &format!("/{name}"));
        package_fixture(
            &doc,
            include_bytes!("../../fixtures/wasi-http-read-request-then-respond.wasm"),
            Directory::default(),
            &authority,
        )
    };
    let dependencies = json!([
        {"publisher":"alice.dhttp.net","name":"first","version":"1.0.0"},
        {"publisher":"alice.dhttp.net","name":"second","version":"1.0.0"}
    ]);
    let parent = package("parent", "1.0.0", dependencies.clone());
    assert!(check_lib(&parent, &runtime).is_ok());
    assert!(install_lib(&profile, "alice.dhttp.net/parent", &parent, &runtime).is_err());
    for name in ["first", "second"] {
        install_lib(
            &profile,
            &format!("alice.dhttp.net/{name}"),
            &package(name, "1.0.0", json!([])),
            &runtime,
        )
        .unwrap();
    }
    let value = install_lib(&profile, "alice.dhttp.net/parent", &parent, &runtime).unwrap();
    assert_eq!(value["dependencies"], dependencies);
    let db = rusqlite::Connection::open(profile.config_db_path()).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM lib_dependencies WHERE lib_name='parent'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        2
    );
    let blocked = remove_lib(&profile, "alice.dhttp.net/first").unwrap_err();
    assert!(blocked.to_string().contains("lib alice.dhttp.net/parent"));
    assert!(
        install_lib(
            &profile,
            "alice.dhttp.net/first",
            &package("first", "2.0.0", json!([])),
            &runtime
        )
        .is_err()
    );
    let mut sandbox = Sandbox::new(runtime.clone());
    sandbox.load_libs(&profile).unwrap();
    std::fs::write(profile.join("db/first/keep.db"), b"keep").unwrap();
    db.execute(
        "UPDATE lib_dependencies SET version='2.0.0' WHERE dependency_name='first'",
        [],
    )
    .unwrap();
    // Startup trusts installation; it does not audit out-of-band DB edits.
    sandbox.load_libs(&profile).unwrap();
    assert_eq!(sandbox.libs.len(), 3);
    db.execute(
        "UPDATE lib_dependencies SET version='1.0.0' WHERE dependency_name='first'",
        [],
    )
    .unwrap();
    remove_lib(&profile, "alice.dhttp.net/parent").unwrap();
    assert!(profile.join("libs/first.wasm").is_file());
    assert_eq!(
        std::fs::read(profile.join("db/first/keep.db")).unwrap(),
        b"keep"
    );
    remove_lib(&profile, "alice.dhttp.net/second").unwrap();
    let first_to_second = package(
        "first",
        "1.0.0",
        json!([{"publisher":"alice.dhttp.net","name":"second","version":"1.0.0"}]),
    );
    install_lib(
        &profile,
        "alice.dhttp.net/second",
        &package("second", "1.0.0", json!([])),
        &runtime,
    )
    .unwrap();
    install_lib(
        &profile,
        "alice.dhttp.net/first",
        &first_to_second,
        &runtime,
    )
    .unwrap();
    // Startup does not repeat dependency-cycle validation after installation.
    db.execute(
        "INSERT INTO lib_dependencies VALUES('second','first','1.0.0')",
        [],
    )
    .unwrap();
    // Startup trusts installation; it does not audit out-of-band DB edits.
    sandbox.load_libs(&profile).unwrap();
    db.execute("DELETE FROM lib_dependencies WHERE lib_name='second'", [])
        .unwrap();
    let self_dependency = package(
        "self",
        "1.0.0",
        json!([{"publisher":"alice.dhttp.net","name":"self","version":"1.0.0"}]),
    );
    let before = package("self", "1.0.0", json!([]));
    install_lib(&profile, "alice.dhttp.net/self", &before, &runtime).unwrap();
    let saved_snapshot = files::disk_bytes(
        &profile,
        &files::lib_root(&profile).unwrap(),
        "alice.dhttp.net/self",
    )
    .unwrap();
    assert!(install_lib(&profile, "alice.dhttp.net/self", &self_dependency, &runtime).is_err());
    assert_eq!(
        files::disk_bytes(
            &profile,
            &files::lib_root(&profile).unwrap(),
            "alice.dhttp.net/self"
        )
        .unwrap(),
        saved_snapshot
    );
    let mut duplicate = dependencies.clone();
    duplicate
        .as_array_mut()
        .unwrap()
        .push(dependencies[0].clone());
    assert!(read_package(&package("invalid", "1.0.0", duplicate)).is_err());
}

#[test]
fn app_installation_and_proxy_writes_check_mount_conflicts_in_transaction() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let uri = "/std/sys/forward".parse().unwrap();
    let app = frontend_package("1.0.0", "installed");
    let id = "alice.dhttp.net/hello";
    for location in [
        "/hello",
        "/hello/",
        "/hello/assets",
        "= /hello",
        "= /hello/assets",
    ] {
        crate::setup::config_database(
            &profile,
            &http::Method::PUT,
            &uri,
            Some(json!([{"location":location,"proxy_pass":"127.0.0.1:8080"}])),
        )
        .unwrap();
        assert!(
            install_app(&profile, id, &app)
                .unwrap_err()
                .to_string()
                .contains("proxy")
        );
        assert!(!profile.join("apps/hello").exists());
    }
    let ancestors = json!([
        {"location":"/","proxy_pass":"127.0.0.1:8080"},
        {"location":"/hel","proxy_pass":"127.0.0.1:8080"},
        {"location":"/hello-adjacent","proxy_pass":"127.0.0.1:8080"}
    ]);
    let expected =
        crate::setup::config_database(&profile, &http::Method::PUT, &uri, Some(ancestors)).unwrap();
    install_app(&profile, id, &app).unwrap();
    for location in [
        "/hello",
        "/hello/",
        "/hello/assets",
        "= /hello",
        "= /hello/assets",
    ] {
        assert!(
            crate::setup::config_database(
                &profile,
                &http::Method::PATCH,
                &uri,
                Some(json!({"location":location,"proxy_pass":"127.0.0.1:8080"}))
            )
            .is_err()
        );
    }
    assert_eq!(
        crate::setup::config_database(&profile, &http::Method::GET, &uri, None).unwrap(),
        expected
    );
    // Out-of-band database edits cannot silently hide an installed App.
    let db = rusqlite::Connection::open(profile.config_db_path()).unwrap();
    db.execute(
        "INSERT INTO proxy_locations(location,proxy_pass) VALUES('/hello','127.0.0.1:8080')",
        [],
    )
    .unwrap();
    assert!(crate::setup::load_server_config(&profile).is_err());
}

#[test]
fn old_business_prefixes_remain_proxyable() {
    let directory = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(directory.path().join("alice")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let uri = "/std/sys/forward".parse().unwrap();
    crate::setup::config_database(
        &profile,
        &http::Method::PUT,
        &uri,
        Some(json!([
            {"location":"/api/hello","proxy_pass":"127.0.0.1:8080"},
            {"location":"/app/hello","proxy_pass":"127.0.0.1:8080"}
        ])),
    )
    .unwrap();
    for path in ["/std", "/std/message", "/std/workspace/"] {
        assert!(
            crate::setup::config_database(
                &profile,
                &http::Method::PATCH,
                &uri,
                Some(json!({"location":path,"proxy_pass":"127.0.0.1:8080"}))
            )
            .is_err()
        );
    }
}

#[tokio::test]
async fn legacy_capability_prefixes_are_projected_then_migrated_without_changing_acl() {
    use tower::ServiceExt;
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let db = rusqlite::Connection::open(profile.config_db_path()).unwrap();
    db.execute_batch("DROP TABLE lib_capability_operations")
        .unwrap();
    db.execute_batch(&include_str!("../../../src/package/packages.sql").replace(
        "substr(path, 1, 1) = '/' AND length(path) > 1",
        "substr(path, 1, 5) = '/std/'",
    ))
    .unwrap();
    db.execute_batch("INSERT INTO libs(name,author,version) VALUES('journal','alice.dhttp.net','1.0.0'),('message','std.dhttp.net','1.0.0');
        INSERT INTO lib_capabilities VALUES('journal','write','Journal'),('message','write','Message');
        INSERT INTO lib_capability_operations VALUES('journal','write','POST','/std/entries'),('message','write','POST','/std/message');").unwrap();
    let acl = rusqlite::Connection::open(profile.access_db_path()).unwrap();
    acl.execute_batch("CREATE TABLE preserved_rules(api TEXT); INSERT INTO preserved_rules VALUES('/std/entries')").unwrap();
    for migrated in [false, true] {
        if migrated {
            // A normal management write upgrades the constraint and path facts.
            remove_lib(&profile, "missing").unwrap();
        }
        let response = crate::routes::capabilities_router(profile.clone())
            .oneshot(
                Request::builder()
                    .uri("/std/acl/promissions")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let value: Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(
            value,
            json!({"alice.dhttp.net/journal · Journal":{"/journal/entries":["POST"]},"std.dhttp.net/message · Message":{"/std/message":["POST"]}})
        );
        let stored: String = db
            .query_row(
                "SELECT path FROM lib_capability_operations WHERE lib_name='journal'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            stored,
            if migrated {
                "/journal/entries"
            } else {
                "/std/entries"
            }
        );
        assert_eq!(
            acl.query_row("SELECT api FROM preserved_rules", [], |row| row
                .get::<_, String>(0))
                .unwrap(),
            "/std/entries"
        );
    }
    assert_eq!(
        db.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap(),
        0
    );
    // New prefixes can be stored and the uniqueness constraint still applies.
    db.execute(
        "INSERT INTO lib_capability_operations VALUES('journal','write','GET','/journal/std/xxxx')",
        [],
    )
    .unwrap();
    assert!(db.execute("INSERT INTO lib_capability_operations VALUES('message','write','GET','/journal/std/xxxx')", []).is_err());
    remove_lib(&profile, "missing").unwrap();
}

#[tokio::test]
async fn ordinary_lib_mounts_bind_signatures_capabilities_and_proxy_conflicts() {
    use http::{Method, StatusCode};
    use tower::ServiceExt;
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let mut doc = json!({"openapi":"3.1.0","info":{"title":"Journal","version":"1.0.0","description":"test","license":{"name":"MIT"}},
        "x-pishoo":{"kind":"lib","publisher":"alice.dhttp.net","name":"journal","dependencies":[],"host":{"request-identity":"identity"},
            "access-groups":{"journal":{"title":"Journal","operations":["write"]}}},
        "paths":{"/std/xxxx":{"post":{"operationId":"write","summary":"write","x-pishoo":{"access-group":"journal"}}},"/contact":{"get":{"operationId":"contact","summary":"local operation"}}}});
    let backend = include_bytes!("../../fixtures/wasi-http-read-request-then-respond.wasm");
    let authority = publisher("alice.dhttp.net", 'a');
    let bytes = package_fixture(&doc, backend, Directory::default(), &authority);
    assert!(read_package(&bytes).is_ok());
    // Even the standard publisher cannot occupy a native reserved path.
    doc["x-pishoo"]["publisher"] = json!("std.dhttp.net");
    assert!(
        read_package(&package_fixture(
            &doc,
            backend,
            Directory::default(),
            &publisher("std.dhttp.net", 's')
        ))
        .is_err()
    );
    // A permitted standard path still needs the actual standard publisher signature.
    doc["paths"] = json!({"/journal":doc["paths"]["/std/xxxx"].clone()});
    assert!(
        read_package(&package_fixture(
            &doc,
            backend,
            Directory::default(),
            &authority
        ))
        .unwrap_err()
        .to_string()
        .contains("signer certificate and publisher disagree")
    );
    let uri = "/std/sys/forward".parse().unwrap();
    let id = "alice.dhttp.net/journal";
    for location in ["/journal", "/journal/", "= /journal/std/xxxx"] {
        crate::setup::config_database(
            &profile,
            &Method::PUT,
            &uri,
            Some(json!([{"location":location,"proxy_pass":"127.0.0.1:8080"}])),
        )
        .unwrap();
        assert!(
            install_lib(&profile, id, &bytes, &runtime)
                .unwrap_err()
                .to_string()
                .contains("proxy")
        );
    }
    crate::setup::config_database(
        &profile,
        &Method::PUT,
        &uri,
        Some(json!([
            {"location":"/","proxy_pass":"127.0.0.1:8080"},
            {"location":"/journalish","proxy_pass":"127.0.0.1:8080"}
        ])),
    )
    .unwrap();
    let installed = install_lib(&profile, id, &bytes, &runtime).unwrap();
    assert!(
        installed["endpoints"]
            .as_array()
            .unwrap()
            .iter()
            .any(|endpoint| endpoint["path"] == "/journal/std/xxxx")
    );
    for location in ["/journal", "/journal/", "= /journal/std/xxxx"] {
        assert!(
            crate::setup::config_database(
                &profile,
                &Method::PATCH,
                &uri,
                Some(json!({"location":location,"proxy_pass":"127.0.0.1:8080"}))
            )
            .is_err()
        );
    }
    let db = rusqlite::Connection::open(profile.config_db_path()).unwrap();
    assert_eq!(
        db.query_row("SELECT path FROM lib_capability_operations", [], |row| {
            row.get::<_, String>(0)
        })
        .unwrap(),
        "/journal/std/xxxx"
    );
    let directory = crate::routes::capabilities_router(profile.clone())
        .oneshot(
            Request::builder()
                .uri("/std/acl/promissions")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let value: Value =
        serde_json::from_slice(&directory.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(
        value,
        json!({"alice.dhttp.net/journal · Journal":{"/journal/std/xxxx":["POST"]}})
    );
    let mut sandbox = Sandbox::new(runtime.clone());
    sandbox.load_libs(&profile).unwrap();
    let router = api_router(&sandbox, crate::test_identity::endpoint("alice"));
    for (path, expected) in [
        ("/journal/std/xxxx", StatusCode::INTERNAL_SERVER_ERROR),
        ("/std/xxxx", StatusCode::NOT_FOUND),
    ] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(path)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
    let mut frontend = frontend_document("1.0.0");
    frontend["x-pishoo"]["name"] = json!("journal");
    frontend["x-pishoo"]["dependencies"] = json!([]);
    install_app(
        &profile,
        id,
        &package_fixture(&frontend, &[], frontend_files("Journal page"), &authority),
    )
    .unwrap();
    sandbox.load_libs(&profile).unwrap();
    let router = api_router(&sandbox, crate::test_identity::endpoint("alice"));
    // The same URL may serve an App GET and a declared Lib POST.
    let page = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/journal/std/xxxx")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    assert_eq!(
        page.into_body().collect().await.unwrap().to_bytes(),
        "Journal page"
    );
    let post = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/journal/std/xxxx")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(post.status(), StatusCode::INTERNAL_SERVER_ERROR);
    db.execute(
        "INSERT INTO proxy_locations VALUES('/journal/blocked','127.0.0.1:8080')",
        [],
    )
    .unwrap();
    assert!(crate::setup::load_server_config(&profile).is_err());
}

#[tokio::test]
async fn std_capabilities_are_atomic_persistent_and_routes_have_one_provider() {
    use http::{Method, StatusCode};
    use tower::ServiceExt;
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.join("libs")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let authority = publisher("std.dhttp.net", 'a');
    let make_lib = |name: &str, version: &str, path: &str, method: &str, title: &str| {
        let doc = json!({"openapi":"3.1.0","info":{"title":name,"version":version,"description":"test","license":{"name":"MIT"}},
            "x-pishoo":{"kind":"lib","publisher":"std.dhttp.net","name":name,"dependencies":[],"host":{"request-identity":"identity"},
                "access-groups":{"message":{"title":title,"operations":["receive"]}}},
            "paths":{path:{method:{"operationId":"receive","summary":"test","x-pishoo":{"access-group":"message"},"responses":{"200":{"description":"ok"}}}}}});
        package_fixture(
            &doc,
            include_bytes!("../../fixtures/wasi-http-read-request-then-respond.wasm"),
            Directory::default(),
            &authority,
        )
    };
    let id = "std.dhttp.net/message";
    let bytes = make_lib("message", "1.0.0", "/message", "post", "1v1聊天");
    install_lib(&profile, id, &bytes, &runtime).unwrap();
    let db = rusqlite::Connection::open(profile.config_db_path()).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT capability_id,title FROM lib_capabilities WHERE lib_name=?1",
            [id.rsplit('/').next().unwrap()],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        )
        .unwrap(),
        ("message".into(), "1v1聊天".into())
    );
    assert_eq!(
        db.query_row(
            "SELECT method,path FROM lib_capability_operations",
            [],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        )
        .unwrap(),
        ("POST".into(), "/std/message".into())
    );
    // Another Lib cannot divide methods of the same business path.
    assert!(
        install_lib(
            &profile,
            "std.dhttp.net/other",
            &make_lib("other", "1.0.0", "/message", "get", "Other"),
            &runtime
        )
        .is_err()
    );
    assert!(!profile.join("libs/other.wasm").exists());
    // Native services cannot become WASM capabilities.
    for path in [
        "/acl/capabilities",
        "/contact",
        "/sys/package/libs",
        "/pishoo/settings",
        "/file/test",
        "/dhttp/test",
    ] {
        assert!(read_package(&make_lib("other", "1.0.0", path, "post", "Other")).is_err());
    }
    let saved_snapshot =
        files::disk_bytes(&profile, &files::lib_root(&profile).unwrap(), id).unwrap();
    db.execute_batch("CREATE TRIGGER reject_capability BEFORE INSERT ON lib_capability_operations BEGIN SELECT RAISE(ABORT,'injected registration failure'); END;").unwrap();
    assert!(
        install_lib(
            &profile,
            id,
            &make_lib("message", "2.0.0", "/message", "post", "Changed"),
            &runtime
        )
        .is_err()
    );
    assert_eq!(
        files::disk_bytes(&profile, &files::lib_root(&profile).unwrap(), id).unwrap(),
        saved_snapshot
    );
    assert_eq!(
        db.query_row("SELECT title FROM lib_capabilities", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "1v1聊天"
    );
    db.execute_batch("DROP TRIGGER reject_capability").unwrap();
    let mut sandbox = Sandbox::new(runtime.clone());
    sandbox.load_libs(&profile).unwrap();
    db.execute("UPDATE lib_capabilities SET title='tampered'", [])
        .unwrap();
    // Startup trusts installation; it does not audit out-of-band DB edits.
    sandbox.load_libs(&profile).unwrap();
    assert_eq!(sandbox.libs.len(), 1);
    install_lib(&profile, id, &bytes, &runtime).unwrap();
    // Static App mounts and Lib business routes have independent roots.
    let mut doc = frontend_document("1.0.0");
    doc["x-pishoo"]["publisher"] = json!("std.dhttp.net");
    doc["x-pishoo"]["kind"] = json!("app");
    doc["x-pishoo"]["name"] = json!("message");
    doc["x-pishoo"]["dependencies"] = json!([]);
    let app = package_fixture(&doc, &[], frontend_files("page"), &authority);
    install_app(&profile, id, &app).unwrap();
    install_lib(
        &profile,
        id,
        &make_lib("message", "2.0.0", "/message", "get", "1v1聊天"),
        &runtime,
    )
    .unwrap();
    install_lib(&profile, id, &bytes, &runtime).unwrap();
    sandbox.load_libs(&profile).unwrap();
    let router = api_router(&sandbox, crate::test_identity::endpoint("alice"));
    for (method, path, status) in [
        (Method::GET, "/message/", StatusCode::OK),
        (
            Method::POST,
            "/std/message",
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        (
            Method::DELETE,
            "/std/message",
            StatusCode::METHOD_NOT_ALLOWED,
        ),
        (Method::GET, "/std/message/missing", StatusCode::NOT_FOUND),
    ] {
        assert_eq!(
            router
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .body(axum::body::Body::empty())
                        .unwrap()
                )
                .await
                .unwrap()
                .status(),
            status
        );
    }
    remove_lib(&profile, id).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM lib_capabilities", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM lib_capability_operations", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap(),
        0
    );
    let access = access_control::AccessService::load_from_db(
        &format!("sqlite://{}?mode=rwc", profile.access_db_path().display()),
        "std.dhttp.net",
        &access_control::SubjectId::new(b"owner").unwrap(),
    )
    .await
    .unwrap();
    let grantee = access_control::Grantee::Named;
    access
        .set_policy(
            access_control::Method::Specified(Method::POST),
            "/std/message",
            access_control::Effect::Allow,
            grantee.clone(),
        )
        .await
        .unwrap();
    // Uninstall kept the path ACL; a fresh provider cannot inherit it silently.
    assert!(
        install_lib(&profile, id, &bytes, &runtime)
            .unwrap_err()
            .to_string()
            .contains("existing ACL")
    );
    access
        .remove_policy(
            access_control::Method::Specified(Method::POST),
            "/std/message",
            grantee,
        )
        .await
        .unwrap();
    install_lib(&profile, id, &bytes, &runtime).unwrap();
    // Startup does not repeat a database-wide foreign-key audit.
    db.pragma_update(None, "foreign_keys", false).unwrap();
    db.execute(
        "INSERT INTO lib_capabilities VALUES('missing','test','Broken')",
        [],
    )
    .unwrap();
    // Startup trusts installation; it does not audit out-of-band DB edits.
    sandbox.load_libs(&profile).unwrap();
    db.execute("DELETE FROM lib_capabilities WHERE lib_name='missing'", [])
        .unwrap();
}

#[test]
fn ordinary_legacy_apps_remain_readable_after_another_package_migrates_registration() {
    for expanded in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
        crate::setup::load_server_config(&profile).unwrap();
        std::fs::create_dir_all(profile.join("libs")).unwrap();
        let db = legacy_schema(&profile);
        let id = "alice.dhttp.net/hello";
        db.execute("INSERT INTO apps VALUES(?1,'hello','1.0.0')", [id])
            .unwrap();
        let text = "<h1>Legacy deployed page 2026</h1>";
        let bytes = frontend_package("1.0.0", text);
        let app = profile.join("apps/hello");
        std::fs::create_dir_all(app.join("public/assets")).unwrap();
        std::fs::write(app.join("public/index.html"), text).unwrap();
        std::fs::write(app.join("public/assets/app.js"), b"fetch('./hello')").unwrap();
        let marker = if expanded {
            let mut spans = [
                ("public/index.html", text.as_bytes()),
                ("public/assets/app.js", b"fetch('./hello')".as_slice()),
            ]
            .into_iter()
            .map(|(path, data)| {
                let start = bytes
                    .windows(data.len())
                    .position(|part| part == data)
                    .unwrap();
                (
                    path,
                    webc::v3::Span {
                        start,
                        len: data.len(),
                    },
                )
            })
            .collect::<Vec<_>>();
            spans.sort_by_key(|(_, span)| span.start);
            let header = serde_json::to_vec(&(bytes.len(), &spans)).unwrap();
            let mut proof = b"pishoo-expanded-webc-v1\0".to_vec();
            proof.extend_from_slice(&(header.len() as u64).to_le_bytes());
            proof.extend_from_slice(&header);
            let mut cursor = 0;
            for (_, span) in spans {
                proof.extend_from_slice(&bytes[cursor..span.start]);
                cursor = span.end();
            }
            proof.extend_from_slice(&bytes[cursor..]);
            std::fs::write(app.join(".webc-proof"), &proof).unwrap();
            proof
        } else {
            std::fs::write(app.join("app.webc"), &bytes).unwrap();
            bytes.clone()
        };
        remove_lib(&profile, "absent").unwrap();
        let fields: (Option<String>, Option<String>) = db
            .query_row(
                "SELECT description,license FROM apps WHERE name='hello'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            fields,
            (None, None),
            "migration must not fabricate source metadata"
        );
        let installed = installed_apps(&profile, Some(id)).unwrap();
        assert_eq!(
            package_file(&installed[id], "/public/index.html").unwrap(),
            text.as_bytes()
        );
        let mut sandbox = Sandbox::new(Arc::new(WasmRuntime::new().unwrap()));
        sandbox.load_libs(&profile).unwrap();
        assert_eq!(
            package_file(&sandbox.apps[id], "/public/index.html").unwrap(),
            text.as_bytes()
        );
        assert_eq!(
            std::fs::read(app.join(if expanded { ".webc-proof" } else { "app.webc" })).unwrap(),
            marker
        );
        remove_app(&profile, id).unwrap();
        assert!(
            !app.exists(),
            "legacy App removal still targets its actual directory"
        );
    }
}

#[test]
fn explicit_management_discards_intermediate_integrity_columns() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let id = "alice.dhttp.net/hello";
    install_app(&profile, id, &frontend_package("1.0.0", "first")).unwrap();
    let db = rusqlite::Connection::open(profile.config_db_path()).unwrap();
    for table in ["libs", "apps"] {
        let count: i64 = db
            .query_row(
                &format!("SELECT count(*) FROM pragma_table_info('{table}')"),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 7, "fresh {table}");
        db.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN package_sha256 TEXT; ALTER TABLE {table} ADD COLUMN author_owner_hash TEXT;")).unwrap();
    }
    db.execute("UPDATE apps SET package_sha256='obsolete hash',author_owner_hash='obsolete owner' WHERE name='hello'", []).unwrap();
    let replacement = package_fixture(
        &frontend_document("1.0.0"),
        &[],
        frontend_files("updated"),
        &publisher("alice.dhttp.net", 'b'),
    );
    install_app(&profile, id, &replacement).unwrap();
    for table in ["libs", "apps"] {
        let columns = db
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            columns,
            [
                "name",
                "author",
                "version",
                "title",
                "description",
                "license",
                "repository"
            ]
        );
    }
    assert_eq!(
        std::fs::read(profile.join("apps/hello/public/index.html")).unwrap(),
        b"updated"
    );
}
