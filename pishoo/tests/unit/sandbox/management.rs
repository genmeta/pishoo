use std::os::unix::fs::MetadataExt;

use axum::body::Body as AxumBody;
use http::{Method, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use super::*;

fn component(version: &str) -> Vec<u8> {
    let mut bytes =
        include_bytes!("../../fixtures/wasi-http-read-request-then-respond.wasm").to_vec();
    let document = serde_json::json!({"openapi":"3.1.0","info":{"title":"Test","version":version},"paths":{"/upload":{"post":{"summary":"Upload","responses":{"200":{"description":"ok"}}}}}}).to_string();
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
    bytes
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
    assert_eq!(saved["endpoints"][0]["path"], "/api/note/upload");
    assert_eq!(saved["endpoints"][0]["description"], "Upload");
    assert!(!profile.join("db/note").exists());
    assert_eq!(
        std::fs::metadata(profile.join("lib/note")).unwrap().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(profile.join("lib/note/lib.wasm"))
            .unwrap()
            .mode()
            & 0o777,
        0o600
    );
    let mut sandbox = Sandbox::new(runtime.clone());
    sandbox.load_libs(&profile).unwrap();
    std::fs::write(profile.join("db/note/saved.db"), b"keep").unwrap();
    let before = std::fs::read(profile.join("lib/note/lib.wasm")).unwrap();
    // A manifest-only component passes metadata validation, but has no incoming handler.
    let mut metadata_only = b"\0asm\x0d\0\x01\0".to_vec();
    let full = component("broken");
    // Append only the final custom section (the fixture has no pishoo manifest).
    let marker = full
        .windows(14)
        .position(|w| w == b"pishoo:openapi")
        .unwrap();
    let length_bytes = if full.len() - marker + 1 >= 128 { 2 } else { 1 };
    metadata_only.extend_from_slice(&full[marker - 1 - length_bytes - 1..]);
    assert!(validate_lib(&metadata_only).is_ok());
    assert!(install_lib(&profile, "note", &metadata_only, &runtime).is_err());
    assert_eq!(
        std::fs::read(profile.join("lib/note/lib.wasm")).unwrap(),
        before
    );
    install_lib(&profile, "note", &component("2"), &runtime).unwrap();
    assert_eq!(sandbox.libs["note"].openapi.info.version, "1");
    assert_eq!(
        installed_libs(&profile, Some("note")).unwrap()["version"],
        "2"
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
    assert_eq!(sandbox.libs["note"].openapi.info.version, "3");
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
    std::fs::write(profile.join("lib/broken/lib.wasm"), b"bad").unwrap();
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
    std::fs::write(outside.join("lib.wasm"), b"do not change").unwrap();
    std::os::unix::fs::symlink(&outside, profile.join("lib/escape")).unwrap();
    assert!(install_lib(&profile, "escape", &component("1"), &runtime).is_err());
    assert!(remove_lib(&profile, "escape").is_err());
    std::fs::remove_file(profile.join("lib/escape")).unwrap();
    std::os::unix::fs::symlink(
        outside.join("lib.wasm"),
        profile.join("lib/broken/lib.wasm-link"),
    )
    .unwrap();
    std::fs::remove_file(profile.join("lib/broken/lib.wasm")).unwrap();
    std::fs::rename(
        profile.join("lib/broken/lib.wasm-link"),
        profile.join("lib/broken/lib.wasm"),
    )
    .unwrap();
    assert!(install_lib(&profile, "broken", &component("1"), &runtime).is_err());
    assert!(remove_lib(&profile, "broken").is_err());
    assert_eq!(
        std::fs::read(outside.join("lib.wasm")).unwrap(),
        b"do not change"
    );
    // Cleanup must unlink nested symlinks without following them.
    std::os::unix::fs::symlink(&outside, profile.join("lib/valid/foreign")).unwrap();
    remove_lib(&profile, "valid").unwrap();
    assert!(outside.join("lib.wasm").exists());
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
            &qtls::default_provider(),
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
        .header("content-type", "application/wasm")
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
async fn lib_routes_enforce_owner_protocol_limits_and_disk_semantics() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    let app = lib_management_router(
        profile.clone(),
        endpoint(),
        Arc::new(WasmRuntime::new().unwrap()),
    );
    for (method, path, body, expected) in [
        (
            Method::POST,
            "/pishoo/lib-check",
            AxumBody::from(component("1")),
            StatusCode::OK,
        ),
        (
            Method::PUT,
            "/pishoo/libs/check",
            AxumBody::from(component("1")),
            StatusCode::OK,
        ),
        (
            Method::GET,
            "/pishoo/libs/check",
            AxumBody::empty(),
            StatusCode::OK,
        ),
        (
            Method::DELETE,
            "/pishoo/libs/check",
            AxumBody::empty(),
            StatusCode::NO_CONTENT,
        ),
        (
            Method::DELETE,
            "/pishoo/libs/check",
            AxumBody::empty(),
            StatusCode::NO_CONTENT,
        ),
        (
            Method::GET,
            "/pishoo/libs/check",
            AxumBody::empty(),
            StatusCode::NOT_FOUND,
        ),
        (
            Method::PUT,
            "/pishoo/libs/note",
            AxumBody::from("bad"),
            StatusCode::BAD_REQUEST,
        ),
        (
            Method::GET,
            "/pishoo/libs?extra=x",
            AxumBody::empty(),
            StatusCode::BAD_REQUEST,
        ),
        (
            Method::GET,
            "/pishoo/libs",
            AxumBody::from("x"),
            StatusCode::BAD_REQUEST,
        ),
        (
            Method::GET,
            "/pishoo/libs/%2Fescape",
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
        if path == "/pishoo/lib-check" {
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
        ("/pishoo/libs", Method::POST, "GET"),
        ("/pishoo/libs/note", Method::PATCH, "GET, PUT, DELETE"),
        ("/pishoo/lib-check", Method::GET, "POST"),
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
        let mut req = request(Method::GET, "/pishoo/libs", AxumBody::empty());
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
        let mut req = request(Method::GET, "/pishoo/libs", AxumBody::empty());
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
        "/pishoo/libs/note",
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
        "/pishoo/libs/note",
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
                "/pishoo/libs/note",
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
            "/pishoo/libs/escape",
            AxumBody::from(component("1"))
        ))
        .await
        .unwrap()
        .status(),
        StatusCode::CONFLICT
    );
}
