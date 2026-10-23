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
    let has_public = !public.children.is_empty();
    let mut api = document.clone();
    let mut annotation = json!({"title":document["info"]["title"],
        "changelog":document["x-pishoo"].get("changelog").cloned().unwrap_or_else(||"Test release; data unchanged".into()),
        "dependencies":document["x-pishoo"].get("dependencies").cloned().unwrap_or_else(||json!([]))});
    if has_public {
        annotation["entry"] = document["x-pishoo"]
            .get("entry")
            .cloned()
            .unwrap_or_else(|| "/".into());
    }
    if !app {
        annotation["host"] = document["x-pishoo"]
            .get("host")
            .cloned()
            .unwrap_or_else(|| json!({}));
    }
    if let Some(extension) = api["x-pishoo"].as_object_mut() {
        extension.retain(|key, _| key == "access-groups");
    }
    let mut backend = backend.to_vec();
    if !app
        && !wasmparser::Parser::new(0).parse_all(&backend).any(
            |p| matches!(p,Ok(wasmparser::Payload::CustomSection(s)) if s.name()=="pishoo:openapi"),
        )
    {
        let name = b"pishoo:openapi";
        let mut section = vec![name.len() as u8];
        section.extend(name);
        section.extend(serde_json::to_vec(&api).unwrap());
        backend.push(0);
        let mut n = section.len();
        while n >= 128 {
            backend.push((n as u8 & 127) | 128);
            n >>= 7;
        }
        backend.push(n as u8);
        backend.extend(section);
    }
    let mut manifest: webc::metadata::Manifest = serde_json::from_value(json!({
        "package":{"wapm":{"name":format!("{}/{}",document["x-pishoo"]["publisher"].as_str().unwrap(),document["x-pishoo"]["name"].as_str().unwrap()),
            "version":document["info"]["version"],"description":document["info"]["description"],"license":document["info"]["license"]["name"]},
            "pishoo":annotation,"fs":if has_public {json!([{"volume_name":"public","mount_path":"/public"}])} else {json!([])}},
        "atoms":{"backend":{"kind":"https://webc.org/kind/wasm","signature":format!("sha256:{}",STANDARD.encode(Sha256::digest(&backend)))}}
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
                FileEntry::borrowed(&backend, Timestamps::default()),
            )])
        }
    };
    let mut writer = Writer::default()
        .write_manifest(&manifest)
        .unwrap()
        .write_atoms(atoms())
        .unwrap();
    if has_public {
        writer.write_volume("public", public).unwrap();
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
            for kind in ["app", "lib"] {
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
    std::fs::create_dir_all(profile.join("lib")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let id = "alice.dhttp.net/hello";
    install_app(&profile, id, &frontend_package("1.0.0", "verified page")).unwrap();
    let directory = profile.join("app/hello");
    assert!(!directory.join("app.webc").exists());
    let index = directory.join("index.html");
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
        let assets = directory.join("assets");
        std::fs::rename(&assets, &outside).unwrap();
        std::os::unix::fs::symlink(&outside, &assets).unwrap();
        assert!(installed_apps(&profile, Some(id)).is_err());
        assert!(outside.join("app.js").is_file());
    }
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
    std::fs::create_dir_all(profile.join("app")).unwrap();
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
            .join("app")
            .join(id.rsplit('/').next().unwrap())
            .join("index.html"),
    )
    .unwrap();
    assert!(install_app(&profile, id, b"broken").is_err());
    assert_eq!(
        std::fs::read(
            profile
                .join("app")
                .join(id.rsplit('/').next().unwrap())
                .join("index.html")
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
                        .uri(format!("/app/{}/?view=1", id.rsplit('/').next().unwrap()))
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
                .uri(format!("/app/{}?value=1", id.rsplit('/').next().unwrap()))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        response.headers()["location"],
        format!("/app/{}/?value=1", id.rsplit('/').next().unwrap())
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
                    .uri(format!("/app/{}{path}", id.rsplit('/').next().unwrap()))
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
                .uri(format!("/app/{}/", id.rsplit('/').next().unwrap()))
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

#[test]
fn backend_dependencies_are_multiple_pinned_persistent_and_acyclic() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.join("lib")).unwrap();
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
    let db =
        crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM package_dependency WHERE package_name='parent'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        2
    );
    let blocked = remove_lib(&profile, "alice.dhttp.net/first").unwrap_err();
    assert!(blocked.to_string().contains("required"));
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
    std::fs::write(profile.join(".volume/first/keep.db"), b"keep").unwrap();
    db.execute(
        "UPDATE package_dependency SET version='2.0.0' WHERE dependency_name='first'",
        [],
    )
    .unwrap();
    // Startup trusts installation; it does not audit out-of-band DB edits.
    sandbox.load_libs(&profile).unwrap();
    assert_eq!(sandbox.libs.len(), 3);
    db.execute(
        "UPDATE package_dependency SET version='1.0.0' WHERE dependency_name='first'",
        [],
    )
    .unwrap();
    remove_lib(&profile, "alice.dhttp.net/parent").unwrap();
    assert!(profile.join("lib/first.wasm").is_file());
    assert_eq!(
        std::fs::read(profile.join(".volume/first/keep.db")).unwrap(),
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
        "INSERT INTO package_dependency VALUES('second','first','1.0.0')",
        [],
    )
    .unwrap();
    // Startup trusts installation; it does not audit out-of-band DB edits.
    sandbox.load_libs(&profile).unwrap();
    db.execute(
        "DELETE FROM package_dependency WHERE package_name='second'",
        [],
    )
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
    let uri = "/std/sys/mount".parse().unwrap();
    let app = frontend_package("1.0.0", "installed");
    let id = "alice.dhttp.net/hello";
    for location in [
        "/app/hello",
        "/app/hello/",
        "/app/hello/assets",
        "= /app/hello",
        "= /app/hello/assets",
    ] {
        crate::setup::config_database(
            &profile,
            &http::Method::PATCH,
            &uri,
            Some(json!({"location":location,"proxy_pass":"127.0.0.1:8080"})),
        )
        .unwrap();
        assert!(
            install_app(&profile, id, &app)
                .unwrap_err()
                .to_string()
                .contains("proxy")
        );
        assert!(!profile.join("app/hello").exists());
        crate::setup::config_database(
            &profile,
            &http::Method::DELETE,
            &format!(
                "/std/sys/mount?location={}&proxy_pass=127.0.0.1%3A8080",
                form_urlencoded::byte_serialize(location.as_bytes()).collect::<String>()
            )
            .parse()
            .unwrap(),
            None,
        )
        .unwrap();
    }
    let ancestors = json!([
        {"location":"/","proxy_pass":"127.0.0.1:8080"},
        {"location":"/app","proxy_pass":"127.0.0.1:8080"},
        {"location":"/app/hello-adjacent","proxy_pass":"127.0.0.1:8080"}
    ]);
    for rule in ancestors.as_array().unwrap() {
        crate::setup::config_database(&profile, &http::Method::PATCH, &uri, Some(rule.clone()))
            .unwrap();
    }
    let expected = crate::setup::config_database(&profile, &http::Method::GET, &uri, None).unwrap();
    install_app(&profile, id, &app).unwrap();
    for location in [
        "/app/hello",
        "/app/hello/",
        "/app/hello/assets",
        "= /app/hello",
        "= /app/hello/assets",
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
    let db =
        crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    db.execute(
        "INSERT INTO proxy_location(location,proxy_pass) VALUES('/app/hello','127.0.0.1:8080')",
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
    let uri = "/std/sys/mount".parse().unwrap();
    for location in ["/api/hello", "/app/hello"] {
        crate::setup::config_database(
            &profile,
            &http::Method::PATCH,
            &uri,
            Some(json!({"location":location,"proxy_pass":"127.0.0.1:8080"})),
        )
        .unwrap();
    }
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
    let db =
        crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    db.execute_batch("DROP TABLE lib_capability_operation")
        .unwrap();
    db.execute_batch(&include_str!("../../../src/package/packages.sql").replace(
        "substr(path, 1, 1) = '/' AND length(path) > 1",
        "substr(path, 1, 5) = '/std/'",
    ))
    .unwrap();
    db.execute_batch("INSERT INTO package(has_lib,has_app,name,author,version) VALUES(1,0,'journal','alice.dhttp.net','1.0.0'),(1,0,'message','std.dhttp.net','1.0.0');
        INSERT INTO lib_capability VALUES('journal','write','Journal'),('message','write','Message');
        INSERT INTO lib_capability_operation VALUES('journal','write','POST','/std/entries'),('message','write','POST','/std/message');").unwrap();
    let acl = rusqlite::Connection::open(profile.join(".volume/sys/access/database")).unwrap();
    acl.execute_batch("CREATE TABLE preserved_rules(api TEXT); INSERT INTO preserved_rules VALUES('/std/entries')").unwrap();
    for migrated in [false, true] {
        if migrated {
            // A normal management write upgrades the constraint and path facts.
            remove_lib(&profile, "missing").unwrap();
        }
        let response = crate::routes::capabilities_router(profile.clone())
            .oneshot(
                Request::builder()
                    .uri("/std/acl/permissions")
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
                "SELECT path FROM lib_capability_operation WHERE lib_name='journal'",
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
        "INSERT INTO lib_capability_operation VALUES('journal','write','GET','/journal/std/xxxx')",
        [],
    )
    .unwrap();
    assert!(db.execute("INSERT INTO lib_capability_operation VALUES('message','write','GET','/journal/std/xxxx')", []).is_err());
    remove_lib(&profile, "missing").unwrap();
}

#[test]
fn standard_lib_declarations_stay_in_the_filename_namespace() {
    let authority = publisher("std.dhttp.net", 'a');
    let backend = include_bytes!("../../fixtures/wasi-http-read-request-then-respond.wasm");
    for (path, accepted) in [
        ("/profile", true),
        ("/profile/", true),
        ("/profile/nickname", true),
        ("/profile-other", false),
        ("/message", false),
    ] {
        let document = json!({
            "openapi":"3.1.0",
            "info":{"title":"Profile","version":"1.0.0","description":"test","license":{"name":"MIT"}},
            "x-pishoo":{"kind":"lib","publisher":"std.dhttp.net","name":"profile","host":{},"dependencies":[]},
            "paths":{path:{"get":{"operationId":"read","summary":"Read","responses":{"200":{"description":"ok"}}}}}
        });
        let bytes = package_fixture(&document, backend, Directory::default(), &authority);
        let result = read_package(&bytes);
        if accepted {
            result.unwrap();
        } else {
            assert!(
                result.unwrap_err().to_string().contains("package name"),
                "{path}"
            );
        }
    }
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
    let uri = "/std/sys/mount".parse().unwrap();
    let id = "alice.dhttp.net/journal";
    for location in ["/journal", "/journal/", "= /journal/std/xxxx"] {
        crate::setup::config_database(
            &profile,
            &Method::PATCH,
            &uri,
            Some(json!({"location":location,"proxy_pass":"127.0.0.1:8080"})),
        )
        .unwrap();
        assert!(
            install_lib(&profile, id, &bytes, &runtime)
                .unwrap_err()
                .to_string()
                .contains("proxy")
        );
        crate::setup::config_database(
            &profile,
            &Method::DELETE,
            &format!(
                "/std/sys/mount?location={}&proxy_pass=127.0.0.1%3A8080",
                form_urlencoded::byte_serialize(location.as_bytes()).collect::<String>()
            )
            .parse()
            .unwrap(),
            None,
        )
        .unwrap();
    }
    for location in ["/", "/journalish"] {
        crate::setup::config_database(
            &profile,
            &Method::PATCH,
            &uri,
            Some(json!({"location":location,"proxy_pass":"127.0.0.1:8080"})),
        )
        .unwrap();
    }
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
    let db =
        crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    assert_eq!(
        db.query_row("SELECT path FROM lib_capability_operation", [], |row| {
            row.get::<_, String>(0)
        })
        .unwrap(),
        "/journal/std/xxxx"
    );
    let directory = crate::routes::capabilities_router(profile.clone())
        .oneshot(
            Request::builder()
                .uri("/std/acl/permissions")
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
    let mut frontend = doc.clone();
    frontend["paths"] = json!({"/std/xxxx":doc["paths"]["/journal"].clone()});
    frontend["x-pishoo"]["publisher"] = "alice.dhttp.net".into();
    install_package(
        &profile,
        id,
        &package_fixture(
            &frontend,
            backend,
            frontend_files("Journal page"),
            &authority,
        ),
        &runtime,
    )
    .unwrap();
    sandbox.load_libs(&profile).unwrap();
    let router = api_router(&sandbox, crate::test_identity::endpoint("alice"));
    // App pages and the same package's backend use separate namespaces.
    let page = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/app/journal/std/xxxx")
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
        "INSERT INTO proxy_location VALUES('/journal/blocked','127.0.0.1:8080')",
        [],
    )
    .unwrap();
    assert!(crate::setup::load_server_config(&profile).is_err());
}

#[test]
fn access_groups_validate_modern_and_legacy_declarations() {
    let document = json!({"openapi":"3.1.0","info":{"title":"Message","version":"1.0.0","description":"test","license":{"name":"MIT"}},
        "x-pishoo":{"kind":"lib","publisher":"std.dhttp.net","name":"message","dependencies":[],"host":{"request-identity":"identity"}},
        "x-access-groups":{"message":{"title":"Receive","operations":["receive"]}},
        "paths":{"/message":{"post":{"operationId":"receive","summary":"Receive messages","x-access-group":"message","responses":{"200":{"description":"ok"}}}}}});
    let authority = publisher("std.dhttp.net", 'a');
    let package = |doc: &Value| {
        package_fixture(
            doc,
            include_bytes!("../../fixtures/wasi-http-read-request-then-respond.wasm"),
            Directory::default(),
            &authority,
        )
    };
    assert!(read_package(&package(&document)).is_ok());
    let mut translated = document.clone();
    translated["x-access-groups"]["message"]["title"] = json!({"en":"Receive", "zh-CN":"接收消息"});
    assert!(read_package(&package(&translated)).is_ok());
    for title in [
        json!({}),
        json!({"en":"Receive"}),
        json!({"en":42,"zh-CN":"消息"}),
        json!({"en":" ","zh-CN":"消息"}),
        json!({"en":"Receive","zh-CN":"消息","fr":"Messages"}),
    ] {
        translated["x-access-groups"]["message"]["title"] = title;
        assert!(read_package(&package(&translated)).is_err());
    }
    let mut legacy = document.clone();
    legacy["x-pishoo"]["access-groups"] = legacy
        .as_object_mut()
        .unwrap()
        .remove("x-access-groups")
        .unwrap();
    legacy["paths"]["/message"]["post"]["x-pishoo"] = json!({"access-group":"message"});
    legacy["paths"]["/message"]["post"]
        .as_object_mut()
        .unwrap()
        .remove("x-access-group");
    assert!(read_package(&package(&legacy)).is_ok());

    // Invalid new declarations must fail installation rather than disappear
    // from the permission catalog as if no capabilities had been declared.
    for (pointer, value, message) in [
        (
            "/x-access-groups",
            json!([]),
            "access-groups must be an object",
        ),
        (
            "/x-access-groups",
            Value::Null,
            "access-groups must be an object",
        ),
        (
            "/x-access-groups/message/operations",
            json!(["missing"]),
            "unknown group operation",
        ),
        (
            "/x-access-groups/message/operations",
            json!(["receive", "receive"]),
            "overlapping or duplicate group operation",
        ),
        (
            "/x-access-groups/message/operations",
            json!([]),
            "group must contain operations",
        ),
        (
            "/paths/~1message/post/x-access-group",
            json!("other"),
            "access-groups disagree with operation annotations",
        ),
        (
            "/paths/~1message/post/x-access-group",
            json!(["message"]),
            "invalid access-group",
        ),
        (
            "/paths/~1message/post/x-access-group",
            Value::Null,
            "invalid access-group",
        ),
    ] {
        let mut invalid = document.clone();
        *invalid.pointer_mut(pointer).unwrap() = value;
        assert!(
            read_package(&package(&invalid))
                .unwrap_err()
                .to_string()
                .contains(message),
            "{pointer}: {message}"
        );
    }
    for field in ["x-access-groups", "x-access-group"] {
        let mut invalid = document.clone();
        if field == "x-access-groups" {
            invalid["x-pishoo"]["access-groups"] = document[field].clone();
        } else {
            invalid["paths"]["/message"]["post"]["x-pishoo"] = json!({"access-group":"message"});
        }
        assert!(
            read_package(&package(&invalid))
                .unwrap_err()
                .to_string()
                .contains("duplicate access-group"),
            "{field}"
        );
    }
    let mut invalid = document.clone();
    invalid.as_object_mut().unwrap().remove("x-access-groups");
    assert!(read_package(&package(&invalid)).is_err());
    let mut invalid = document;
    invalid["paths"]["/message"]["post"]
        .as_object_mut()
        .unwrap()
        .remove("x-access-group");
    assert!(read_package(&package(&invalid)).is_err());
}

#[tokio::test]
async fn std_capabilities_are_atomic_persistent_and_routes_have_one_provider() {
    use http::{Method, StatusCode};
    use tower::ServiceExt;
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.join("lib")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let authority = publisher("std.dhttp.net", 'a');
    let make_lib = |name: &str, version: &str, path: &str, method: &str, title: &str| {
        let doc = json!({"openapi":"3.1.0","info":{"title":name,"version":version,"description":"test","license":{"name":"MIT"}},
            "x-pishoo":{"kind":"lib","publisher":"std.dhttp.net","name":name,"dependencies":[],"host":{"request-identity":"identity"}},
            "x-access-groups":{"message":{"title":title,"operations":["receive"]}},
            "paths":{path:{method:{"operationId":"receive","summary":"test","x-access-group":"message","responses":{"200":{"description":"ok"}}}}}});
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
    let db =
        crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT capability_id,title FROM lib_capability WHERE lib_name=?1",
            [id.rsplit('/').next().unwrap()],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        )
        .unwrap(),
        ("message".into(), "1v1聊天".into())
    );
    assert_eq!(
        db.query_row(
            "SELECT method,path FROM lib_capability_operation",
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
    assert!(!profile.join("lib/other.wasm").exists());
    // Native services cannot become WASM capabilities.
    for path in [
        "/acl/capabilities",
        "/contact",
        "/sys/package/lib",
        "/pishoo/settings",
        "/file/test",
        "/dhttp/test",
    ] {
        assert!(read_package(&make_lib("other", "1.0.0", path, "post", "Other")).is_err());
    }
    let saved_snapshot =
        files::disk_bytes(&profile, &files::lib_root(&profile).unwrap(), id).unwrap();
    db.execute_batch("CREATE TRIGGER acl.reject_capability BEFORE INSERT ON lib_capability_operation BEGIN SELECT RAISE(ABORT,'injected registration failure'); END;").unwrap();
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
        db.query_row("SELECT title FROM lib_capability", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "1v1聊天"
    );
    db.execute_batch("DROP TRIGGER reject_capability").unwrap();
    let mut sandbox = Sandbox::new(runtime.clone());
    sandbox.load_libs(&profile).unwrap();
    db.execute("UPDATE lib_capability SET title='tampered'", [])
        .unwrap();
    // Startup trusts installation; it does not audit out-of-band DB edits.
    sandbox.load_libs(&profile).unwrap();
    assert_eq!(sandbox.libs.len(), 1);
    install_lib(&profile, id, &bytes, &runtime).unwrap();
    let doc = json!({"openapi":"3.1.0","info":{"title":"Message","version":"1.0.0","description":"test","license":{"name":"MIT"}},
        "x-pishoo":{"publisher":"std.dhttp.net","name":"message","host":{"request-identity":"identity"}},
        "x-access-groups":{"message":{"title":"1v1聊天","operations":["receive"]}},
        "paths":{"/message":{"post":{"operationId":"receive","summary":"Receive","x-access-group":"message"}}}});
    let combined = package_fixture(
        &doc,
        include_bytes!("../../fixtures/wasi-http-read-request-then-respond.wasm"),
        frontend_files("page"),
        &authority,
    );
    install_package(&profile, id, &combined, &runtime).unwrap();
    sandbox.load_libs(&profile).unwrap();
    let router = api_router(&sandbox, crate::test_identity::endpoint("alice"));
    for (method, path, status) in [
        (Method::GET, "/app/message/", StatusCode::OK),
        (
            Method::POST,
            "/std/message",
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        (
            Method::DELETE,
            "/std/message",
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        (
            Method::GET,
            "/std/message/missing",
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
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
        db.query_row("SELECT count(*) FROM lib_capability", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM lib_capability_operation", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap(),
        0
    );
    db.execute_batch("DROP TABLE acl.lib_capability_operation; DROP TABLE acl.lib_capability;")
        .unwrap();
    let access = access_control::AccessService::load_from_db(
        &format!(
            "sqlite://{}?mode=rwc",
            profile.join(".volume/sys/access/database").display()
        ),
        "std.dhttp.net",
        &access_control::SubjectId::new(b"owner").unwrap(),
    )
    .await
    .unwrap();
    db.execute_batch(include_str!("../../../src/package/packages.sql"))
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
        "INSERT INTO lib_capability VALUES('missing','test','Broken')",
        [],
    )
    .unwrap();
    // Startup trusts installation; it does not audit out-of-band DB edits.
    sandbox.load_libs(&profile).unwrap();
    db.execute("DELETE FROM lib_capability WHERE lib_name='missing'", [])
        .unwrap();
}

#[tokio::test]
async fn combined_package_serves_pages_and_backend_methods_and_is_one_identity() {
    use tower::ServiceExt;
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("owner")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let mut doc = frontend_document("1.0.0");
    doc["x-pishoo"]["kind"] = "lib".into();
    doc["paths"] = json!({"/events":{"post":{"operationId":"events","summary":"Events"}}});
    let bytes = package_fixture(
        &doc,
        include_bytes!("../../fixtures/wasi-http-read-request-then-respond.wasm"),
        frontend_files("page"),
        &publisher("alice.dhttp.net", 'a'),
    );
    install_package(&profile, "alice.dhttp.net/hello", &bytes, &runtime).unwrap();
    let mut sandbox = Sandbox::new(runtime.clone());
    sandbox.load_libs(&profile).unwrap();
    let router = api_router(&sandbox, crate::test_identity::endpoint("alice.dhttp.net"));
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/app/hello/")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), http::StatusCode::OK);
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/hello/events")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(response.status(), http::StatusCode::METHOD_NOT_ALLOWED);
    let db =
        crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM package", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    let mut foreign = doc.clone();
    foreign["x-pishoo"]["publisher"] = "bob.dhttp.net".into();
    let foreign = package_fixture(
        &foreign,
        &[],
        frontend_files("other"),
        &publisher("bob.dhttp.net", 'b'),
    );
    assert!(install_package(&profile, "bob.dhttp.net/hello", &foreign, &runtime).is_err());
}

#[test]
fn package_catalog_uses_registration_without_a_payload_audit() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("owner")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let db =
        crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    db.execute("INSERT INTO package(name,author,version,title,has_lib,has_app) VALUES('hello','alice.dhttp.net','1.0.0','Hello',1,1)",[]).unwrap();
    let value = installed_catalog(&profile, Some("hello")).unwrap();
    assert_eq!(value["id"], "alice.dhttp.net/hello");
    assert_eq!(value["has_lib"], true);
    assert_eq!(value["has_app"], true);
    assert!(value.get("kind").is_none());
    assert!(value["endpoints"].as_array().unwrap().is_empty());
    let columns = db
        .prepare("PRAGMA table_info(package)")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(columns.len(), 11);
    for removed in ["id", "package_sha256", "author_owner_hash", "changelog"] {
        assert!(!columns.iter().any(|c| c == removed));
    }
}
