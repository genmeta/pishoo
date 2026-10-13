use std::os::unix::fs::MetadataExt;

use axum::body::Body as AxumBody;
use http::{Method, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use super::*;

fn component(version: &str) -> Vec<u8> {
    let mut bytes =
        include_bytes!("../../fixtures/wasi-http-read-request-then-respond.wasm").to_vec();
    let document = serde_json::json!({"openapi":"3.1.0","info":{"title":"Test","version":version},"x-pishoo":{"publisher":"alice.dhttp.net","name":"note","host":{}},"paths":{"/upload":{"post":{"operationId":"upload","summary":"Upload","responses":{"200":{"description":"ok"}}}}}}).to_string();
    let mut section = vec![14];
    section.extend_from_slice(b"pishoo:openapi");
    section.extend_from_slice(document.as_bytes());
    bytes.push(0);
    let mut n = section.len();
    loop {
        let b = (n & 127) as u8;
        n >>= 7;
        bytes.push(b | if n > 0 { 128 } else { 0 });
        if n == 0 {
            break;
        }
    }
    bytes.extend(section);
    package_component(&bytes)
}
fn profile(root: &Path) -> IdentityProfile {
    let profile = IdentityProfile::try_from(root.join("alice")).unwrap();
    std::fs::create_dir_all(profile.join("lib")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    profile
}
#[test]
fn install_checks_compatibility_and_preserves_loaded_versions_data_and_old_bytes() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let saved = install_lib(&profile, "note", &component("1"), &runtime).unwrap();
    assert_eq!(saved["endpoints"][0]["path"], "/std/api/note/upload");
    assert_eq!(saved["endpoints"][0]["description"], "Upload");
    assert_eq!(
        saved["openapi"],
        serde_json::to_value(check_lib(&component("1"), &runtime).unwrap()).unwrap()
    );
    assert!(!profile.join("db/note").exists());
    assert_eq!(
        std::fs::metadata(profile.join("lib/note")).unwrap().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(profile.join("lib/note/lib.webc"))
            .unwrap()
            .mode()
            & 0o777,
        0o600
    );
    let mut sandbox = Sandbox::new(runtime.clone());
    sandbox.load_libs(&profile).unwrap();
    std::fs::write(profile.join("db/note/saved.db"), b"keep").unwrap();
    let before = std::fs::read(profile.join("lib/note/lib.webc")).unwrap();
    // A manifest-only component passes metadata validation, but has no incoming handler.
    let metadata_only = package_fixture(
        &serde_json::json!({"openapi":"3.1.0","info":{"title":"Test","version":"1.0.0","description":"Test","license":{"name":"MIT"}},"x-pishoo":{"publisher":"alice.dhttp.net","name":"note","host":{}},"paths":{"/upload":{"post":{"operationId":"upload","summary":"Upload","responses":{"200":{"description":"ok"}}}}}}),
        b"\0asm\x0d\0\x01\0",
        webc::v3::write::Directory::default(),
        &publisher("alice.dhttp.net", 'a'),
    );
    assert!(read_package(&metadata_only).is_ok());
    assert!(install_lib(&profile, "note", &metadata_only, &runtime).is_err());
    assert_eq!(
        std::fs::read(profile.join("lib/note/lib.webc")).unwrap(),
        before
    );
    install_lib(&profile, "note", &component("2"), &runtime).unwrap();
    assert_eq!(sandbox.libs["note"].openapi.info.version, "1.0.0");
    assert_eq!(
        installed_libs(&profile, Some("note")).unwrap()["openapi"]["info"]["version"],
        "2.0.0"
    );
    assert_eq!(
        installed_libs(&profile, Some("note")).unwrap()["version"],
        "2.0.0"
    );
    assert_eq!(
        std::fs::read_dir(profile.join("lib/note")).unwrap().count(),
        1
    );
    remove_lib(&profile, "note").unwrap();
    remove_lib(&profile, "note").unwrap();
    assert!(sandbox.libs.contains_key("note"));
    assert_eq!(
        std::fs::read(profile.join("db/note/saved.db")).unwrap(),
        b"keep"
    );
    sandbox.load_libs(&profile).unwrap();
    assert!(sandbox.libs.is_empty());
    install_lib(&profile, "note", &component("3"), &runtime).unwrap();
    sandbox.load_libs(&profile).unwrap();
    assert_eq!(sandbox.libs["note"].openapi.info.version, "3.0.0");
    assert_eq!(
        std::fs::read(profile.join("db/note/saved.db")).unwrap(),
        b"keep"
    );
}
#[test]
fn disk_catalog_reports_invalid_components_and_rejects_layout_escapes() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    let runtime = WasmRuntime::new().unwrap();
    install_lib(&profile, "valid", &component("1"), &runtime).unwrap();
    std::fs::create_dir(profile.join("lib/broken")).unwrap();
    std::fs::write(profile.join("lib/broken/lib.webc"), b"bad").unwrap();
    let list = installed_libs(&profile, None).unwrap();
    assert!(list[0].get("error").is_some());
    assert!(list[0].get("title").is_none());
    assert_eq!(list[1]["id"], "valid");
    assert!(matches!(
        installed_libs(&profile, Some("broken")),
        Err(Error::InvalidComponent(_))
    ));
    let outside = root.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("lib.webc"), b"do not change").unwrap();
    std::os::unix::fs::symlink(&outside, profile.join("lib/escape")).unwrap();
    assert!(install_lib(&profile, "escape", &component("1"), &runtime).is_err());
    assert!(remove_lib(&profile, "escape").is_err());
    std::fs::remove_file(profile.join("lib/escape")).unwrap();
    std::os::unix::fs::symlink(
        outside.join("lib.webc"),
        profile.join("lib/broken/lib.webc-link"),
    )
    .unwrap();
    std::fs::remove_file(profile.join("lib/broken/lib.webc")).unwrap();
    std::fs::rename(
        profile.join("lib/broken/lib.webc-link"),
        profile.join("lib/broken/lib.webc"),
    )
    .unwrap();
    assert!(install_lib(&profile, "broken", &component("1"), &runtime).is_err());
    assert!(remove_lib(&profile, "broken").is_err());
    assert_eq!(
        std::fs::read(outside.join("lib.webc")).unwrap(),
        b"do not change"
    );
    // Cleanup must unlink nested symlinks without following them.
    std::os::unix::fs::symlink(&outside, profile.join("lib/valid/foreign")).unwrap();
    remove_lib(&profile, "valid").unwrap();
    assert!(outside.join("lib.webc").exists());
    for id in ["../outside", "A", "", "note.wasm", "%6Eote"] {
        assert!(remove_lib(&profile, id).is_err());
    }
    std::fs::rename(profile.join("lib"), profile.join("real-lib")).unwrap();
    std::os::unix::fs::symlink(profile.join("real-lib"), profile.join("lib")).unwrap();
    assert!(install_lib(&profile, "valid", &component("1"), &runtime).is_err());
    assert!(installed_libs(&profile, None).is_err());
}
#[test]
fn concurrent_installs_queries_and_removal_publish_whole_files() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    let runtime = WasmRuntime::new().unwrap();
    std::thread::scope(|scope| {
        for version in ["one", "two", "three"] {
            let profile = &profile;
            let runtime = &runtime;
            scope
                .spawn(move || install_lib(profile, "note", &component(version), runtime).unwrap());
        }
    });
    std::thread::scope(|scope| {
        let profile = &profile;
        let runtime = &runtime;
        scope.spawn(move || {
            for _ in 0..3 {
                install_lib(profile, "note", &component("updated"), runtime).unwrap();
            }
        });
        scope.spawn(move || {
            for _ in 0..3 {
                remove_lib(profile, "note").unwrap();
            }
        });
        scope.spawn(move || {
            for _ in 0..10 {
                let list = installed_libs(profile, None).unwrap();
                assert!(
                    list.as_array()
                        .unwrap()
                        .iter()
                        .all(|entry| entry.get("error").is_none())
                );
            }
        });
    });
    assert!(std::fs::read_dir(profile.path()).unwrap().all(|e| {
        !e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".pishoo-")
    }));
    let root_fd = lib_root(&profile).unwrap();
    rustix::fs::flock(&root_fd, rustix::fs::FlockOperation::LockExclusive).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            tx.send(installed_libs(&profile, None)).unwrap();
        });
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
        rustix::fs::flock(&root_fd, rustix::fs::FlockOperation::Unlock).unwrap();
        rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
    });
}
fn endpoint() -> dhttp::Endpoint {
    let name = "alice.dhttp.net";
    let mut params = rcgen::CertificateParams::new(vec![name.into()]).unwrap();
    let ski = format!("0:0:{}", "a".repeat(64));
    let mut der = vec![4, ski.len() as u8];
    der.extend_from_slice(ski.as_bytes());
    params
        .custom_extensions
        .push(rcgen::CustomExtension::from_oid_content(
            &[2, 5, 29, 14],
            der,
        ));
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = params.self_signed(&key).unwrap();
    dhttp::Endpoint::new(
        qbase::endpoint::Endpoint::new(
            name,
            vec![cert.der().clone()],
            qtls::PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
            vec![1],
        )
        .unwrap(),
    )
}
fn request(method: Method, path: &str, body: AxumBody) -> Request<AxumBody> {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/webc")
        .body(body)
        .unwrap();
    request
        .extensions_mut()
        .insert(access_control::Visitor::new(
            "alice.dhttp.net",
            access_control::SubjectId::new("a".repeat(64).as_bytes()).unwrap(),
        ));
    request
}

#[tokio::test]
async fn complete_id_routes_and_store_install_inputs_are_owner_only_and_immediate() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    let app = lib_management_router(
        profile.clone(),
        endpoint(),
        Arc::new(RwLock::new(Sandbox::new(Arc::new(
            WasmRuntime::new().unwrap(),
        )))),
    );
    let path = "/std/pishoo/libs/alice.dhttp.net/note";
    let response = app
        .clone()
        .oneshot(request(
            Method::PUT,
            path,
            AxumBody::from(component("1.2.3")),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = app
        .clone()
        .oneshot(request(Method::GET, path, AxumBody::empty()))
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(value["id"], "alice.dhttp.net/note");
    assert_eq!(
        value["endpoints"][0]["path"],
        "/std/api/alice.dhttp.net/note/upload"
    );
    assert!(profile.join("db/alice.dhttp.net/note").is_dir());
    for bad in [
        "/std/pishoo/libs/alice.dhttp.net%2Fnote",
        "/std/pishoo/libs/alice.dhttp.net/note%2Fextra",
    ] {
        assert_eq!(
            app.clone()
                .oneshot(request(Method::GET, bad, AxumBody::empty()))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    let path = "/std/pishoo/store-install";
    assert_eq!(
        app.clone()
            .oneshot(request(Method::GET, path, AxumBody::empty()))
            .await
            .unwrap()
            .status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
    for (payload, status) in [
        (
            serde_json::json!({"publisher":"alice.dhttp.net","name":"note","version":"01.2.3"}),
            StatusCode::BAD_REQUEST,
        ),
        (
            serde_json::json!({"publisher":"alice.dhttp.net","name":"../note","version":"1.2.3"}),
            StatusCode::BAD_REQUEST,
        ),
        (
            serde_json::json!({"publisher":"alice.dhttp.net","name":"note","version":"1.2.3","url":"https://example.com"}),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let mut req = request(Method::POST, path, AxumBody::from(payload.to_string()));
        req.headers_mut()
            .insert("content-type", "application/json".parse().unwrap());
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), status);
    }
    let mut req = request(Method::POST, path, AxumBody::from("{}"));
    req.extensions_mut().remove::<access_control::Visitor>();
    assert_eq!(
        app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.clone()
            .oneshot(request(Method::POST, path, AxumBody::from("{}")))
            .await
            .unwrap()
            .status(),
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    let mut req = request(Method::POST, path, AxumBody::from("x".repeat(4097)));
    req.headers_mut()
        .insert("content-type", "application/json".parse().unwrap());
    assert_eq!(
        app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
}
#[tokio::test]
async fn lib_routes_enforce_owner_protocol_limits_and_disk_semantics() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    let app = lib_management_router(
        profile.clone(),
        endpoint(),
        Arc::new(RwLock::new(Sandbox::new(Arc::new(
            WasmRuntime::new().unwrap(),
        )))),
    );
    for (method, path, body, expected) in [
        (
            Method::POST,
            "/std/pishoo/lib-check",
            AxumBody::from(component("1")),
            StatusCode::OK,
        ),
        (
            Method::PUT,
            "/std/pishoo/libs/check",
            AxumBody::from(component("1")),
            StatusCode::OK,
        ),
        (
            Method::GET,
            "/std/pishoo/libs/check",
            AxumBody::empty(),
            StatusCode::OK,
        ),
        (
            Method::DELETE,
            "/std/pishoo/libs/check",
            AxumBody::empty(),
            StatusCode::NO_CONTENT,
        ),
        (
            Method::DELETE,
            "/std/pishoo/libs/check",
            AxumBody::empty(),
            StatusCode::NO_CONTENT,
        ),
        (
            Method::GET,
            "/std/pishoo/libs/check",
            AxumBody::empty(),
            StatusCode::NOT_FOUND,
        ),
        (
            Method::PUT,
            "/std/pishoo/libs/note",
            AxumBody::from("bad"),
            StatusCode::BAD_REQUEST,
        ),
        (
            Method::GET,
            "/std/pishoo/libs?extra=x",
            AxumBody::empty(),
            StatusCode::BAD_REQUEST,
        ),
        (
            Method::GET,
            "/std/pishoo/libs",
            AxumBody::from("x"),
            StatusCode::BAD_REQUEST,
        ),
        (
            Method::GET,
            "/std/pishoo/libs/%2Fescape",
            AxumBody::empty(),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let response = app
            .clone()
            .oneshot(request(method, path, body))
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{path}");
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert_eq!(response.headers()["supported-versions"], "v1");
        if path == "/std/pishoo/lib-check" {
            let value: Value = serde_json::from_slice(
                &axum::body::to_bytes(response.into_body(), 65536)
                    .await
                    .unwrap(),
            )
            .unwrap();
            assert!(value.get("id").is_none());
            assert_eq!(value["endpoints"][0]["path"], "/upload");
        }
    }
    for (path, method, allow) in [
        ("/std/pishoo/libs", Method::POST, "GET"),
        ("/std/pishoo/libs/note", Method::PATCH, "GET, PUT, DELETE"),
        ("/std/pishoo/lib-check", Method::GET, "POST"),
    ] {
        let response = app
            .clone()
            .oneshot(request(method, path, AxumBody::empty()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(response.headers()["allow"], allow);
    }
    for version in ["v2", "v2, v1"] {
        let mut req = request(Method::GET, "/std/pishoo/libs", AxumBody::empty());
        req.headers_mut()
            .insert("accept-versions", version.parse().unwrap());
        assert_eq!(
            app.clone().oneshot(req).await.unwrap().status(),
            if version == "v2" {
                StatusCode::HTTP_VERSION_NOT_SUPPORTED
            } else {
                StatusCode::OK
            }
        );
    }
    for visitor in [
        None,
        Some(access_control::Visitor::new(
            "bob.dhttp.net",
            access_control::SubjectId::new("a".repeat(64).as_bytes()).unwrap(),
        )),
        Some(access_control::Visitor::new(
            "alice.dhttp.net",
            access_control::SubjectId::new("b".repeat(64).as_bytes()).unwrap(),
        )),
    ] {
        let mut req = request(Method::GET, "/std/pishoo/libs", AxumBody::empty());
        req.extensions_mut().remove::<access_control::Visitor>();
        if let Some(visitor) = visitor {
            req.extensions_mut().insert(visitor);
        }
        assert_eq!(
            app.clone().oneshot(req).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
    }
    let mut req = request(
        Method::PUT,
        "/std/pishoo/libs/note",
        AxumBody::from(component("1")),
    );
    req.headers_mut()
        .insert("content-type", "application/json".parse().unwrap());
    assert_eq!(
        app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    let chunks = futures::stream::iter(
        (0..65).map(|_| Ok::<_, std::io::Error>(Bytes::from(vec![0u8; 1024 * 1024]))),
    );
    let mut req = request(
        Method::PUT,
        "/std/pishoo/libs/note",
        AxumBody::from_stream(chunks),
    );
    req.headers_mut()
        .insert("content-length", "1".parse().unwrap());
    assert_eq!(
        app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let broken = futures::stream::iter([
        Ok(Bytes::from(component("1"))),
        Err(std::io::Error::other("upload interrupted")),
    ]);
    assert_eq!(
        app.clone()
            .oneshot(request(
                Method::PUT,
                "/std/pishoo/libs/note",
                AxumBody::from_stream(broken)
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert!(!profile.join("lib/note").exists());
    std::os::unix::fs::symlink(root.path(), profile.join("lib/escape")).unwrap();
    assert_eq!(
        app.oneshot(request(
            Method::PUT,
            "/std/pishoo/libs/escape",
            AxumBody::from(component("1"))
        ))
        .await
        .unwrap()
        .status(),
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn legacy_wasm_cannot_be_uploaded_as_a_webc_release() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    let app = lib_management_router(
        profile.clone(),
        endpoint(),
        Arc::new(RwLock::new(Sandbox::new(Arc::new(
            WasmRuntime::new().unwrap(),
        )))),
    );
    let legacy = include_bytes!("../../fixtures/wasi-http-read-request-then-respond.wasm");
    for (content_type, expected) in [
        ("application/wasm", StatusCode::UNSUPPORTED_MEDIA_TYPE),
        ("application/webc", StatusCode::BAD_REQUEST),
    ] {
        let mut input = request(
            Method::PUT,
            "/std/pishoo/libs/note",
            AxumBody::from(legacy.as_slice()),
        );
        input
            .headers_mut()
            .insert("content-type", content_type.parse().unwrap());
        let response = app.clone().oneshot(input).await.unwrap();
        assert_eq!(response.status(), expected);
    }
    assert!(!profile.join("lib/note").exists());
}

#[tokio::test]
async fn live_install_update_and_uninstall_share_one_router_and_keep_inflight_requests() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    let endpoint = endpoint();
    let sandbox = Arc::new(RwLock::new(Sandbox::new(Arc::new(
        WasmRuntime::new().unwrap(),
    ))));
    let app = Sandbox::api_router(sandbox.clone(), endpoint.clone()).merge(lib_management_router(
        profile.clone(),
        endpoint.clone(),
        sandbox.clone(),
    ));
    let package = |version: &str, text: &'static str, backend: &[u8]| {
        let document = serde_json::json!({"openapi":"3.1.0","info":{"title":"Live app","version":version,"description":"Test","license":{"name":"MIT"}},
            "x-pishoo":{"publisher":"alice.dhttp.net","name":"note","entry":"/","host":{}},
            "paths":{"/":{"get":{"operationId":"page","summary":"Page","x-pishoo":{"static-file":"/public/index.html"}},"head":{"operationId":"pageHead","summary":"Page headers","x-pishoo":{"static-file":"/public/index.html"}}},
            "/run":{"post":{"operationId":"run","summary":"Backend"}}}});
        package_fixture(
            &document,
            backend,
            webc::v3::write::Directory::new(
                std::collections::BTreeMap::from([(
                    "index.html".parse().unwrap(),
                    text.as_bytes().to_vec().into(),
                )]),
                webc::v3::Timestamps::default(),
            ),
            &publisher("alice.dhttp.net", 'a'),
        )
    };
    assert_eq!(
        app.clone()
            .oneshot(request(Method::GET, "/std/api/note/", AxumBody::empty()))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let first = package(
        "1.0.0",
        "old page",
        include_bytes!("../../fixtures/wasi-http-stream-response-until-cancelled.wasm"),
    );
    assert_eq!(
        app.clone()
            .oneshot(request(
                Method::PUT,
                "/std/pishoo/libs/note",
                AxumBody::from(first)
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let old_page = app
        .clone()
        .oneshot(request(Method::GET, "/std/api/note/", AxumBody::empty()))
        .await
        .unwrap();
    let mut incoming = request(Method::POST, "/std/api/note/run", AxumBody::empty());
    *incoming.uri_mut() = "https://alice.dhttp.net/std/api/note/run".parse().unwrap();
    incoming.extensions_mut().insert(dhttp::HandshakeSummary {
        alpn: None,
        local: Some(endpoint.local_authority().unwrap()),
        remote: None,
    });
    let mut old_guest = app.clone().oneshot(incoming).await.unwrap().into_body();
    let tasks = sandbox.read().unwrap().tasks.clone();
    assert!(!tasks.is_empty());
    std::fs::write(profile.join("db/note/private.db"), b"keep data").unwrap();

    let second = package(
        "2.0.0",
        "new page",
        include_bytes!("../../fixtures/wasi-http-read-request-then-respond.wasm"),
    );
    assert_eq!(
        app.clone()
            .oneshot(request(
                Method::PUT,
                "/std/pishoo/libs/note",
                AxumBody::from(second.clone())
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let new_page = app
        .clone()
        .oneshot(request(Method::GET, "/std/api/note/", AxumBody::empty()))
        .await
        .unwrap();
    assert_eq!(
        new_page.into_body().collect().await.unwrap().to_bytes(),
        "new page"
    );
    assert_eq!(
        old_page.into_body().collect().await.unwrap().to_bytes(),
        "old page"
    );
    let catalog = app
        .clone()
        .oneshot(request(
            Method::GET,
            "/std/workspace-api/libs",
            AxumBody::empty(),
        ))
        .await
        .unwrap()
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let catalog: Value = serde_json::from_slice(&catalog).unwrap();
    assert_eq!(catalog[0]["version"], "2.0.0");
    assert_eq!(
        app.clone()
            .oneshot(request(
                Method::PUT,
                "/std/pishoo/libs/note",
                AxumBody::from("broken")
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        std::fs::read(profile.join("lib/note/lib.webc")).unwrap(),
        second
    );
    assert_eq!(
        sandbox.read().unwrap().libs["note"].openapi.info.version,
        "2.0.0"
    );

    assert_eq!(
        app.clone()
            .oneshot(request(
                Method::DELETE,
                "/std/pishoo/libs/note",
                AxumBody::empty()
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        app.clone()
            .oneshot(request(Method::GET, "/std/api/note/", AxumBody::empty()))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert!(sandbox.read().unwrap().libs.is_empty());
    assert_eq!(
        std::fs::read(profile.join("db/note/private.db")).unwrap(),
        b"keep data"
    );
    assert!(!tasks.is_empty());
    assert!(old_guest.frame().await.unwrap().unwrap().is_data());
    drop(old_guest);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !tasks.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    sandbox.write().unwrap().close();
    let third = package(
        "3.0.0",
        "closed",
        include_bytes!("../../fixtures/wasi-http-read-request-then-respond.wasm"),
    );
    assert_eq!(
        app.oneshot(request(
            Method::PUT,
            "/std/pishoo/libs/note",
            AxumBody::from(third)
        ))
        .await
        .unwrap()
        .status(),
        StatusCode::FORBIDDEN
    );
    assert!(!profile.join("lib/note").exists());
}
