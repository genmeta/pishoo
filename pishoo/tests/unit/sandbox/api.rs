use ::http::StatusCode;
use axum::{Router, body::Body as AxumBody};
use http::Method;
use tower::ServiceExt;

use super::*;

fn load(sandbox: &Sandbox, directory: &Path, paths: &str) -> Arc<Lib> {
    fn leb(mut n: usize, output: &mut Vec<u8>) {
        loop {
            let byte = (n & 127) as u8;
            n >>= 7;
            output.push(byte | if n != 0 { 128 } else { 0 });
            if n == 0 {
                break;
            }
        }
    }
    let manifest = format!(
        r#"{{"openapi":"3.1.0","info":{{"title":"HTTP","version":"1.0.0"}},"paths":{paths}}}"#
    );
    let mut section = Vec::new();
    leb(b"pishoo:openapi".len(), &mut section);
    section.extend_from_slice(b"pishoo:openapi");
    section.extend_from_slice(manifest.as_bytes());
    let mut bytes =
        include_bytes!("../../fixtures/wasi-http-stream-response-until-cancelled.wasm").to_vec();
    bytes.push(0);
    leb(section.len(), &mut bytes);
    bytes.extend(section);
    let mut lib = Lib::load(
        sandbox.runtime.clone(),
        "test".into(),
        &package_component(&bytes),
        directory,
    )
    .unwrap();
    // Catalog fallback semantics are tested with the original metadata after
    // the production loader has validated the signed fixture package.
    lib.openapi = validate_lib(&bytes).unwrap();
    Arc::new(lib)
}

fn request(method: Method, path: &str) -> Request<AxumBody> {
    Request::builder()
        .method(method)
        .uri(format!("https://alice.dhttp.net{path}"))
        .body(AxumBody::empty())
        .unwrap()
}

async fn status(router: &Router, method: Method, path: &str) -> StatusCode {
    router
        .clone()
        .oneshot(request(method, path))
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn retired_runtime_inspection_routes_are_not_available() {
    let mut sandbox = Sandbox::new(Arc::new(WasmRuntime::new().unwrap()));
    let directory = tempfile::tempdir().unwrap();
    let lib = load(&sandbox, directory.path(), r#"{"/run":{"get":{}}}"#);
    sandbox.libs.insert("demo".into(), lib);
    let router = api_router(&sandbox, crate::test_identity::endpoint("alice"));
    for path in [
        "/std/sys/lib/loaded",
        "/std/sys/app/loaded",
        "/std/sys/lib/openapi?name=demo",
        "/std/sys/lib/check",
        "/std/sys/app/check",
    ] {
        for method in [Method::GET, Method::POST] {
            assert_eq!(
                status(&router, method, path).await,
                StatusCode::NOT_FOUND,
                "{path}"
            );
        }
    }
    assert!(sandbox.tasks.is_empty());
}

#[tokio::test]
async fn api_methods_require_manifest_entries_and_namespace_never_falls_through() {
    let mut sandbox = Sandbox::new(Arc::new(WasmRuntime::new().unwrap()));
    let directory = tempfile::tempdir().unwrap();
    let lib = load(
        &sandbox,
        directory.path(),
        r#"{"/run":{"get":{},"post":{}},"/explicit":{"head":{},"options":{}}}"#,
    );
    sandbox.libs.insert("test".into(), lib);
    let router = api_router(&sandbox, crate::test_identity::endpoint("alice"));
    for (method, path, expected) in [
        (Method::GET, "/test/run", StatusCode::INTERNAL_SERVER_ERROR),
        (
            Method::POST,
            "/test/run?value=1",
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        (Method::HEAD, "/test/run", StatusCode::METHOD_NOT_ALLOWED),
        (Method::OPTIONS, "/test/run", StatusCode::METHOD_NOT_ALLOWED),
        (
            Method::HEAD,
            "/test/explicit",
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        (
            Method::OPTIONS,
            "/test/explicit",
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    ] {
        assert_eq!(status(&router, method, path).await, expected, "{path}");
    }
    for path in ["/std", "/std/missing", "/std/missing/deep"] {
        for method in [Method::GET, Method::POST, Method::HEAD, Method::OPTIONS] {
            assert_eq!(status(&router, method, path).await, StatusCode::NOT_FOUND);
        }
    }
    assert_eq!(
        status(&router, Method::CONNECT, "/test/run").await,
        StatusCode::METHOD_NOT_ALLOWED
    );
    let router = router.fallback(|| async { StatusCode::IM_A_TEAPOT });
    for path in ["/api/test/run", "/app/workspace/", "/api/websocket"] {
        assert_eq!(
            status(&router, Method::GET, path).await,
            StatusCode::IM_A_TEAPOT
        );
    }
    assert!(sandbox.tasks.is_empty());
}

#[tokio::test]
async fn lib_paths_are_scoped_to_the_registered_publisher_and_name() {
    let mut sandbox = Sandbox::new(Arc::new(WasmRuntime::new().unwrap()));
    let directory = tempfile::tempdir().unwrap();
    let lib = load(
        &sandbox,
        directory.path(),
        r#"{"/profile":{"get":{},"post":{}},"/workspace/context":{"get":{}}}"#,
    );
    sandbox
        .libs
        .insert("publisher.dhttp.net/workspace".into(), lib);
    let router = api_router(&sandbox, crate::test_identity::endpoint("alice"));
    for path in [
        "/workspace/profile",
        "/workspace/profile?search=a%2Fb",
        "/workspace/workspace/context",
    ] {
        assert_eq!(
            status(&router, Method::GET, path).await,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
    assert_eq!(
        status(&router, Method::DELETE, "/workspace/profile").await,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        status(&router, Method::GET, "/std/profile").await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn only_standard_publisher_uses_std_and_other_paths_remain_inside_the_package() {
    let mut sandbox = Sandbox::new(Arc::new(WasmRuntime::new().unwrap()));
    let directory = tempfile::tempdir().unwrap();
    let standard = load(&sandbox, directory.path(), r#"{"/message":{"post":{}}}"#);
    let ordinary = load(
        &sandbox,
        directory.path(),
        r#"{"/std/xxxx":{"post":{}},"/message":{"post":{}}}"#,
    );
    sandbox
        .libs
        .insert("std.dhttp.net/message".into(), standard);
    sandbox
        .libs
        .insert("alice.dhttp.net/journal".into(), ordinary);
    let router = api_router(&sandbox, crate::test_identity::endpoint("alice"));
    for path in [
        "/std/message",
        "/journal/message",
        "/journal/std/xxxx?value=a%2Fb",
    ] {
        assert_eq!(
            status(&router, Method::POST, path).await,
            StatusCode::INTERNAL_SERVER_ERROR,
            "{path}"
        );
        assert_eq!(
            status(&router, Method::GET, path).await,
            StatusCode::METHOD_NOT_ALLOWED,
            "{path}"
        );
    }
    for path in [
        "/std/xxxx",
        "/std/journal/std/xxxx",
        "/journalish/std/xxxx",
        "/journal/missing",
        "/message/message",
    ] {
        assert_eq!(
            status(&router, Method::POST, path).await,
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
}

#[tokio::test]
async fn routers_keep_their_lib_snapshot() {
    let mut sandbox = Sandbox::new(Arc::new(WasmRuntime::new().unwrap()));
    let directory = tempfile::tempdir().unwrap();
    let old = load(&sandbox, directory.path(), r#"{"/old":{"post":{}}}"#);
    sandbox.libs.insert("test".into(), old);
    let endpoint = crate::test_identity::endpoint("alice");
    let old_router = api_router(&sandbox, endpoint.clone());
    let new = load(&sandbox, directory.path(), r#"{"/new":{"post":{}}}"#);
    sandbox.libs.insert("test".into(), new);
    let new_router = api_router(&sandbox, endpoint);
    for (router, present, absent) in [
        (&old_router, "/test/old", "/test/new"),
        (&new_router, "/test/new", "/test/old"),
    ] {
        assert_eq!(
            status(router, Method::POST, present).await,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            status(router, Method::POST, absent).await,
            StatusCode::NOT_FOUND
        );
    }
    sandbox.close();
    assert!(sandbox.libs.is_empty());
}

#[tokio::test]
async fn routed_executions_exceed_four_concurrent_requests_and_survive_close() {
    let mut sandbox = Sandbox::new(Arc::new(WasmRuntime::new().unwrap()));
    let directory = tempfile::tempdir().unwrap();
    let lib = load(&sandbox, directory.path(), r#"{"/run":{"post":{}}}"#);
    sandbox.libs.insert("test".into(), lib);
    let endpoint = crate::test_identity::endpoint("alice");
    let router = api_router(&sandbox, endpoint.clone());
    assert_eq!(
        status(&router, Method::POST, "/test/run").await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert!(sandbox.tasks.is_empty());
    let local = endpoint.local_authority().unwrap();
    let mut responses = Vec::new();
    for index in 0..8 {
        let mut request = request(Method::POST, &format!("/test/run?index={index}"));
        request.extensions_mut().insert(dhttp::HandshakeSummary {
            alpn: None,
            local: Some(local.clone()),
            remote: None,
        });
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        responses.push(response);
    }
    assert_eq!(sandbox.tasks.len(), 8);
    sandbox.close();
    assert_eq!(sandbox.tasks.len(), 8);
    drop(responses);
}
