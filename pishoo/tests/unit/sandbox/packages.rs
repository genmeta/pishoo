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
    let manifest: webc::metadata::Manifest = serde_json::from_value(json!({
        "package":{"wapm":{"name":format!("{}/{}", document["x-pishoo"]["publisher"].as_str().unwrap(),document["x-pishoo"]["name"].as_str().unwrap()), "version":document["info"]["version"]},
            "fs":[{"volume_name":"public","mount_path":"/public"},{"volume_name":"metadata","mount_path":"/metadata"}]},
        "atoms":{"backend":{"kind":"https://webc.org/kind/wasm","signature":format!("sha256:{}",STANDARD.encode(Sha256::digest(backend)))}}
    })).unwrap();
    let mut writer = Writer::default()
        .write_manifest(&manifest)
        .unwrap()
        .write_atoms(BTreeMap::from([(
            "backend".parse().unwrap(),
            FileEntry::borrowed(backend, Timestamps::default()),
        )]))
        .unwrap()
        .with_volume("public", public)
        .unwrap()
        .with_volume(
            "metadata",
            Directory::new(
                BTreeMap::from([(
                    "openapi.json".parse().unwrap(),
                    serde_json::to_vec(document).unwrap().into(),
                )]),
                Timestamps::default(),
            ),
        )
        .unwrap();
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
        .write_atoms(BTreeMap::from([(
            "backend".parse().unwrap(),
            FileEntry::borrowed(backend, Timestamps::default()),
        )]))
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

fn frontend_document(version: &str) -> Value {
    json!({"openapi":"3.1.0","info":{"title":"Hello","version":version,"description":"Hello test","license":{"name":"MIT"}},
    "x-pishoo":{"publisher":"alice.dhttp.net","name":"hello","entry":"/","host":{}},
    "paths":{
        "/":{"get":{"operationId":"page","summary":"Page","x-pishoo":{"static-file":"/public/index.html"}},"head":{"operationId":"pageHead","summary":"Page headers","x-pishoo":{"static-file":"/public/index.html"}}},
        "/assets/app.js":{"get":{"operationId":"js","summary":"Script","x-pishoo":{"static-file":"/public/assets/app.js"}},"head":{"operationId":"jsHead","summary":"Script headers","x-pishoo":{"static-file":"/public/assets/app.js"}}},
        "/hello":{"post":{"operationId":"hello","summary":"Backend"}}
    }})
}
fn frontend_files(text: &'static str) -> Directory<'static> {
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
    for modification in [
        "missing-head",
        "wrong-file",
        "backend-conflict",
        "wrong-entry",
    ] {
        let mut doc = frontend_document("1.0.0");
        match modification {
            "missing-head" => {
                doc["paths"]["/"].as_object_mut().unwrap().remove("head");
            }
            "wrong-file" => {
                doc["paths"]["/"]["get"]["x-pishoo"]["static-file"] =
                    "/metadata/openapi.json".into()
            }
            "backend-conflict" => {
                doc["paths"]["/"]["post"] = json!({"summary":"Conflicting backend"})
            }
            _ => doc["x-pishoo"]["entry"] = "/hello".into(),
        }
        let bad = package_fixture(
            &doc,
            include_bytes!("../../fixtures/wasi-http-read-request-then-respond.wasm"),
            frontend_files("<h1>Old frontend</h1>"),
            &publisher("alice.dhttp.net", 'a'),
        );
        assert!(read_package(&bad).is_err(), "{modification}");
    }
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
async fn frontend_and_backend_share_installed_version_and_static_requests_skip_guest() {
    use http::{Method, StatusCode};
    use tower::ServiceExt;
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.join("lib")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let id = "alice.dhttp.net/hello";
    install_lib(
        &profile,
        id,
        &frontend_package("1.0.0", "old page"),
        &runtime,
    )
    .unwrap();
    let mut sandbox = Sandbox::new(runtime.clone());
    sandbox.load_libs(&profile).unwrap();
    let old = api_router(&sandbox, crate::test_identity::endpoint("alice.dhttp.net"));
    install_lib(
        &profile,
        id,
        &frontend_package("2.0.0", "new page"),
        &runtime,
    )
    .unwrap();
    let before = std::fs::read(profile.join("lib").join(id).join("lib.webc")).unwrap();
    assert!(install_lib(&profile, id, b"broken", &runtime).is_err());
    assert_eq!(
        std::fs::read(profile.join("lib").join(id).join("lib.webc")).unwrap(),
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
                        .uri(format!("/std/api/{id}/?view=1"))
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
                .uri(format!("/std/api/{id}?value=1"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        response.headers()["location"],
        format!("/std/api/{id}/?value=1")
    );
    for (method, path, status) in [
        (Method::POST, "/", StatusCode::METHOD_NOT_ALLOWED),
        (Method::GET, "/missing", StatusCode::NOT_FOUND),
        (Method::GET, "/hello", StatusCode::METHOD_NOT_ALLOWED),
        (Method::POST, "/hello", StatusCode::INTERNAL_SERVER_ERROR),
    ] {
        let response = new
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(format!("/std/api/{id}{path}"))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), status, "{path}");
    }
    remove_lib(&profile, id).unwrap();
    assert!(profile.join("db").join(id).is_dir());
    // Running router owns the old package even after the disk resource is removed.
    let response = old
        .oneshot(
            Request::builder()
                .uri(format!("/std/api/{id}/"))
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
