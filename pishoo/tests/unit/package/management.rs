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
    std::fs::create_dir_all(profile.join("libs")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    profile
}
#[test]
fn install_checks_compatibility_and_preserves_loaded_versions_data_and_old_bytes() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let parent_mode = std::fs::metadata(profile.join("libs")).unwrap().mode() & 0o777;
    let saved = install_lib(&profile, "note", &component("1"), &runtime).unwrap();
    assert_eq!(saved["endpoints"][0]["path"], "/note/upload");
    assert_eq!(saved["endpoints"][0]["description"], "Upload");
    assert_eq!(
        saved["openapi"],
        serde_json::to_value(check_lib(&component("1"), &runtime).unwrap()).unwrap()
    );
    assert!(!profile.join("db/note").exists());
    assert_eq!(
        std::fs::metadata(profile.join("libs")).unwrap().mode() & 0o777,
        parent_mode
    );
    assert_eq!(
        std::fs::metadata(profile.join("libs/note.wasm"))
            .unwrap()
            .mode()
            & 0o777,
        0o600
    );
    let mut sandbox = Sandbox::new(runtime.clone());
    sandbox.load_libs(&profile).unwrap();
    std::fs::write(profile.join("db/note/saved.db"), b"keep").unwrap();
    let before = std::fs::read(profile.join("libs/note.wasm")).unwrap();
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
        std::fs::read(profile.join("libs/note.wasm")).unwrap(),
        before
    );
    install_lib(&profile, "note", &component("2"), &runtime).unwrap();
    assert_eq!(sandbox.libs["note"].openapi.info.version, "1.0.0");
    assert_eq!(
        installed_libs(&profile, Some("note")).unwrap()["version"],
        "2.0.0"
    );
    assert_eq!(std::fs::read_dir(profile.join("libs")).unwrap().count(), 1);
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
    std::fs::create_dir(profile.join("libs/broken")).unwrap();
    std::fs::write(profile.join("libs/broken/.webc-proof"), b"bad").unwrap();
    let list = installed_libs(&profile, None).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["id"], "valid");
    assert!(matches!(
        installed_libs(&profile, Some("broken")),
        Err(Error::RouteNotFound)
    ));
    let db = rusqlite::Connection::open(profile.config_db_path()).unwrap();
    db.execute(
        "INSERT INTO libs(name,version) VALUES('broken','1.0.0')",
        [],
    )
    .unwrap();
    let list = installed_libs(&profile, None).unwrap();
    assert_eq!(list[0]["id"], "broken");
    assert_eq!(list[0]["title"], "broken");
    assert!(list[0]["description"].is_null());
    assert_eq!(list[1]["id"], "valid");
    assert_eq!(
        installed_libs(&profile, Some("broken")).unwrap()["version"],
        "1.0.0"
    );
    let outside = root.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join(".webc-proof"), b"do not change").unwrap();
    std::os::unix::fs::symlink(&outside, profile.join("libs/escape")).unwrap();
    assert!(install_lib(&profile, "escape", &component("1"), &runtime).is_err());
    remove_lib(&profile, "escape").unwrap();
    assert!(
        profile
            .join("libs/escape")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink()
    );
    std::fs::remove_file(profile.join("libs/escape")).unwrap();
    std::os::unix::fs::symlink(
        outside.join(".webc-proof"),
        profile.join("libs/broken/.webc-proof-link"),
    )
    .unwrap();
    std::fs::remove_file(profile.join("libs/broken/.webc-proof")).unwrap();
    std::fs::rename(
        profile.join("libs/broken/.webc-proof-link"),
        profile.join("libs/broken/.webc-proof"),
    )
    .unwrap();
    // The active WASM target must reject symlinks independently of unused
    // legacy sidecars or other Libs' now-isolated routes.
    std::os::unix::fs::symlink(
        outside.join(".webc-proof"),
        profile.join("libs/broken.wasm"),
    )
    .unwrap();
    assert!(install_lib(&profile, "broken", &component("1"), &runtime).is_err());
    assert!(remove_lib(&profile, "broken").is_err());
    assert_eq!(
        std::fs::read(outside.join(".webc-proof")).unwrap(),
        b"do not change"
    );
    // Cleanup must unlink nested symlinks without following them.
    std::os::unix::fs::symlink(&outside, profile.join("libs/foreign")).unwrap();
    remove_lib(&profile, "valid").unwrap();
    assert!(outside.join(".webc-proof").exists());
    for id in ["../outside", "A", "", "note.wasm", "%6Eote"] {
        assert!(remove_lib(&profile, id).is_err());
    }
    std::fs::rename(profile.join("libs"), profile.join("real-lib")).unwrap();
    std::os::unix::fs::symlink(profile.join("real-lib"), profile.join("libs")).unwrap();
    assert!(install_lib(&profile, "valid", &component("1"), &runtime).is_err());
    assert!(installed_libs(&profile, None).is_ok());
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
    install_lib(&profile, "note", &component("updated"), &runtime).unwrap();
    let root_fd = lib_root(&profile).unwrap();
    rustix::fs::flock(&root_fd, rustix::fs::FlockOperation::LockExclusive).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            tx.send(installed_libs(&profile, None)).unwrap();
        });
        // Catalog reads use only committed DB facts and never wait on payloads.
        rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        let (payload_tx, payload_rx) = std::sync::mpsc::channel();
        let payload_profile = &profile;
        let payload_root = &root_fd;
        scope.spawn(move || {
            payload_tx
                .send(disk_bytes(payload_profile, payload_root, "note"))
                .unwrap();
        });
        // The installed snapshot must observe metadata and payload
        // together, so it waits until the publication lock is released.
        assert!(payload_rx.recv_timeout(Duration::from_millis(100)).is_err());
        rustix::fs::flock(&root_fd, rustix::fs::FlockOperation::Unlock).unwrap();
        payload_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
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
        .header("content-type", "application/json")
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
async fn installed_catalog_and_delete_preserve_complete_identity_and_data() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    let mut sandbox = Sandbox::new(Arc::new(WasmRuntime::new().unwrap()));
    // Internal storage helpers seed already-installed resources. Public writes
    // accept only a desired exact store release, never these local bytes.
    install_lib(
        &profile,
        "alice.dhttp.net/note",
        &component("1.2.3"),
        &sandbox.runtime,
    )
    .unwrap();
    sandbox.load_libs(&profile).unwrap();
    let sandbox = Arc::new(RwLock::new(sandbox));
    let router = lib_management_router(profile.clone(), endpoint(), sandbox.clone());
    let path = "/std/sys/lib?name=note";
    let response = router
        .clone()
        .oneshot(request(Method::GET, path, AxumBody::empty()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["package-id"], "alice.dhttp.net/note");
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(response.headers()["supported-versions"], "v1");
    let value: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(value["id"], "alice.dhttp.net/note");
    assert_eq!(value["version"], "1.2.3");
    assert_eq!(
        value["endpoints"],
        serde_json::json!([
            {"method":"POST","path":"/note/upload","description":"Upload"}
        ])
    );
    assert_eq!(value["openapi"]["info"]["version"], "1.2.3");
    assert!(value["openapi"]["paths"].get("/upload").is_some());
    let response = router
        .clone()
        .oneshot(request(Method::GET, "/std/sys/lib/list", AxumBody::empty()))
        .await
        .unwrap();
    let list: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(list[0]["endpoints"], serde_json::json!([]));
    assert!(list[0].get("openapi").is_none());
    let database = rusqlite::Connection::open(profile.config_db_path()).unwrap();
    database
        .execute("UPDATE libs SET version='2.0.0' WHERE name='note'", [])
        .unwrap();
    let response = router
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
    assert_eq!(value["version"], "2.0.0");
    assert!(value.get("openapi").is_none());
    assert_eq!(value["endpoints"], serde_json::json!([]));
    database
        .execute("UPDATE libs SET version='1.2.3' WHERE name='note'", [])
        .unwrap();
    for old in [
        "/std/sys/package/libs",
        "/std/sys/package/libs/note",
        "/std/sys/package/store-install",
        "/std/sys/lib/openapi?name=note",
        "/std/sys/lib/check",
        "/std/sys/app/check",
        "/std/sys/lib/loaded",
        "/std/sys/app/loaded",
    ] {
        assert_eq!(
            router
                .clone()
                .oneshot(request(Method::GET, old, AxumBody::empty()))
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
    }
    let mut wrong = request(Method::DELETE, path, AxumBody::empty());
    wrong
        .headers_mut()
        .insert("expected-package-id", "bob.dhttp.net/note".parse().unwrap());
    assert_eq!(
        router.clone().oneshot(wrong).await.unwrap().status(),
        StatusCode::CONFLICT
    );
    std::fs::write(profile.join("db/note/keep.db"), b"keep").unwrap();
    std::fs::write(profile.join("libs/note.wasm"), b"corrupt runtime payload").unwrap();
    let mut removal = request(Method::DELETE, path, AxumBody::empty());
    removal.headers_mut().insert(
        "expected-package-id",
        "alice.dhttp.net/note".parse().unwrap(),
    );
    assert_eq!(
        router.clone().oneshot(removal).await.unwrap().status(),
        StatusCode::NO_CONTENT
    );
    assert!(sandbox.read().unwrap().libs.is_empty());
    assert_eq!(
        std::fs::read(profile.join("db/note/keep.db")).unwrap(),
        b"keep"
    );
    assert!(!profile.join("libs/note.wasm").exists());
    assert_eq!(
        router
            .clone()
            .oneshot(request(Method::GET, path, AxumBody::empty()))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let mut missing = request(Method::DELETE, path, AxumBody::empty());
    missing.headers_mut().insert(
        "expected-package-id",
        "alice.dhttp.net/note".parse().unwrap(),
    );
    assert_eq!(
        router.clone().oneshot(missing).await.unwrap().status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        router
            .oneshot(request(Method::DELETE, path, AxumBody::empty()))
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn store_put_requires_uri_name_two_json_fields_and_retires_local_upload_and_post() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    let router = lib_management_router(
        profile.clone(),
        endpoint(),
        Arc::new(RwLock::new(Sandbox::new(Arc::new(
            WasmRuntime::new().unwrap(),
        )))),
    );
    for kind in ["lib", "app"] {
        let path = format!("/std/sys/{kind}?name=note");
        for body in [
            "{}",
            "null",
            "[]",
            r#"{"publisher":"alice.dhttp.net"}"#,
            r#"{"publisher":1,"version":"1.0.0"}"#,
            r#"{"publisher":"alice.dhttp.net","version":1}"#,
            r#"{"publisher":"alice.dhttp.net","version":"1.0.0","name":"note"}"#,
            r#"{"publisher":"alice.dhttp.net","version":"1.0.0","kind":"lib"}"#,
            r#"{"publisher":"alice.dhttp.net","version":"1.0.0","url":"https://other"}"#,
            r#"{"publisher":"alice.dhttp.net","version":"01.0.0"}"#,
            r#"{"publisher":"alice","version":"1.0.0"}"#,
        ] {
            let response = router
                .clone()
                .oneshot(request(Method::PUT, &path, AxumBody::from(body)))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{kind} {body}");
            assert_eq!(response.headers()["cache-control"], "no-store");
            assert_eq!(response.headers()["supported-versions"], "v1");
        }
        for content_type in [
            "application/webc",
            "application/octet-stream",
            "application/wasm",
            "text/plain",
        ] {
            let mut raw = request(Method::PUT, &path, AxumBody::from(component("1")));
            raw.headers_mut()
                .insert("content-type", content_type.parse().unwrap());
            assert_eq!(
                router.clone().oneshot(raw).await.unwrap().status(),
                StatusCode::UNSUPPORTED_MEDIA_TYPE
            );
        }
        let mut missing_type = request(Method::PUT, &path, AxumBody::from("{}"));
        missing_type.headers_mut().remove("content-type");
        assert_eq!(
            router.clone().oneshot(missing_type).await.unwrap().status(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
        // The accepted media type permits case/parameters; invalid exact version
        // is rejected before contacting any external store.
        let mut media = request(
            Method::PUT,
            &path,
            AxumBody::from(r#"{"publisher":"alice.dhttp.net","version":"01.0.0"}"#),
        );
        media.headers_mut().insert(
            "content-type",
            "Application/JSON; charset=utf-8".parse().unwrap(),
        );
        let response = router.clone().oneshot(media).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(
            String::from_utf8(
                axum::body::to_bytes(response.into_body(), 4096)
                    .await
                    .unwrap()
                    .to_vec()
            )
            .unwrap()
            .contains("version")
        );
        for uri in [format!("/std/sys/{kind}"), path.clone()] {
            let response = router
                .clone()
                .oneshot(request(
                    Method::POST,
                    &uri,
                    AxumBody::from(
                        r#"{"publisher":"alice.dhttp.net","name":"note","version":"1.0.0"}"#,
                    ),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
            assert_eq!(response.headers()["allow"], "GET, PUT, DELETE");
        }
        let oversized = request(Method::PUT, &path, AxumBody::from(" ".repeat(4097)));
        assert_eq!(
            router.clone().oneshot(oversized).await.unwrap().status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        for uri in [
            format!("/std/sys/{kind}"),
            format!("/std/sys/{kind}?name=note&name=other"),
            format!("/std/sys/{kind}?name=note&version=1.0.0"),
            format!("/std/sys/{kind}?name=alice.dhttp.net%2Fnote"),
            format!("/std/sys/{kind}?name=%FF"),
            format!("/std/sys/{kind}?na%6De=note"),
            format!("/std/sys/{kind}/list?name=note"),
        ] {
            let method = if uri.contains("/list") {
                Method::GET
            } else {
                Method::PUT
            };
            assert_eq!(
                router
                    .clone()
                    .oneshot(request(method, &uri, AxumBody::empty()))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::BAD_REQUEST,
                "{uri}"
            );
        }
        assert_eq!(
            router
                .clone()
                .oneshot(request(Method::DELETE, &path, AxumBody::from("x")))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert!(
        installed_libs(&profile, None)
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(installed_apps(&profile, None).unwrap().is_empty());
    assert!(!profile.join("libs/note.wasm").exists());
}

#[tokio::test]
async fn management_protocol_preserves_owner_version_headers_body_limits_and_closed_lifecycle() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    let sandbox = Arc::new(RwLock::new(Sandbox::new(Arc::new(
        WasmRuntime::new().unwrap(),
    ))));
    let router = lib_management_router(profile.clone(), endpoint(), sandbox.clone());
    for (path, method, allow) in [
        ("/std/sys/lib/list", Method::POST, "GET"),
        ("/std/sys/lib/list", Method::PUT, "GET"),
        ("/std/sys/lib?name=note", Method::PATCH, "GET, PUT, DELETE"),
        ("/std/sys/app?name=note", Method::HEAD, "GET, PUT, DELETE"),
    ] {
        let response = router
            .clone()
            .oneshot(request(method, path, AxumBody::empty()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(response.headers()["allow"], allow);
    }
    for version in ["v2", "v2, v1"] {
        let mut req = request(Method::GET, "/std/sys/lib/list", AxumBody::empty());
        req.headers_mut()
            .insert("accept-versions", version.parse().unwrap());
        assert_eq!(
            router.clone().oneshot(req).await.unwrap().status(),
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
        for method in [Method::GET, Method::PUT] {
            let mut req = request(method, "/std/sys/lib?name=note", AxumBody::from("{}"));
            req.extensions_mut().remove::<access_control::Visitor>();
            if let Some(visitor) = &visitor {
                req.extensions_mut().insert(visitor.clone());
            }
            assert_eq!(
                router.clone().oneshot(req).await.unwrap().status(),
                StatusCode::FORBIDDEN
            );
        }
    }
    for method in [Method::GET, Method::PUT] {
        let mut req = request(method, "/std/sys/lib?name=note", AxumBody::empty());
        req.headers_mut().insert(
            "expected-package-id",
            "alice.dhttp.net/note".parse().unwrap(),
        );
        assert_eq!(
            router.clone().oneshot(req).await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );
    }
    let mut trailer = request(Method::PUT, "/std/sys/lib?name=note", AxumBody::from("{}"));
    trailer
        .headers_mut()
        .insert("trailer", "signature".parse().unwrap());
    assert_eq!(
        router.clone().oneshot(trailer).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
    let broken = futures::stream::iter([
        Ok(Bytes::from_static(b"{")),
        Err(std::io::Error::other("body interrupted")),
    ]);
    assert_eq!(
        router
            .clone()
            .oneshot(request(
                Method::PUT,
                "/std/sys/lib?name=note",
                AxumBody::from_stream(broken)
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        router
            .clone()
            .oneshot(request(
                Method::GET,
                "/std/sys/lib/list",
                AxumBody::from("x")
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    sandbox.write().unwrap().close();
    assert_eq!(
        router
            .clone()
            .oneshot(request(Method::GET, "/std/sys/lib/list", AxumBody::empty()))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        router
            .oneshot(request(
                Method::DELETE,
                "/std/sys/lib?name=note",
                AxumBody::empty()
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn another_authors_same_name_put_preserves_legacy_data_before_store_resolution() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let id = "alice.dhttp.net/note";
    install_lib(&profile, id, &component("1.0.0"), &runtime).unwrap();
    let legacy = profile.join("db/alice.dhttp.net/note");
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::write(legacy.join("keep.db"), b"existing business data").unwrap();
    let data_inode = std::fs::metadata(legacy.join("keep.db")).unwrap().ino();
    assert!(!profile.join("db/note").exists());

    let mut loaded = Sandbox::new(runtime.clone());
    loaded.load_libs(&profile).unwrap();
    let original = loaded.libs[id].clone();
    assert!(!profile.join("db/note").exists());
    let sandbox = Arc::new(RwLock::new(loaded));
    let router = lib_management_router(profile.clone(), endpoint(), sandbox.clone());
    // No store network is initialized by this fixture. The installed-name
    // conflict must be recognized before resolving or downloading bob/note,
    // and before preparing a candidate's new db/note directory.
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        router.oneshot(request(
            Method::PUT,
            "/std/sys/lib?name=note",
            AxumBody::from(
                serde_json::json!({"publisher":"bob.dhttp.net","version":"1.0.0"}).to_string(),
            ),
        )),
    )
    .await
    .expect("same-name rejection must not wait for external store resolution")
    .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let error = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&error).contains("another author"));
    assert!(!profile.join("db/note").exists());
    assert!(Arc::ptr_eq(&original, &sandbox.read().unwrap().libs[id]));
    assert_eq!(
        installed_libs(&profile, Some(id)).unwrap()["version"],
        "1.0.0"
    );

    let mut restarted = Sandbox::new(runtime);
    restarted.load_libs(&profile).unwrap();
    assert_eq!(restarted.libs[id].openapi.info.version, "1.0.0");
    assert!(!profile.join("db/note").exists());
    assert_eq!(
        std::fs::read(legacy.join("keep.db")).unwrap(),
        b"existing business data"
    );
    assert_eq!(
        std::fs::metadata(legacy.join("keep.db")).unwrap().ino(),
        data_inode
    );
}
