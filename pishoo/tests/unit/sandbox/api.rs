use ::http::StatusCode;
use axum::{Router, body::Body as AxumBody};
use tower::ServiceExt;

use super::*;

fn load(sandbox: &Sandbox, directory: &Path, paths: &str, cancel: CancellationToken) -> Arc<Lib> {
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
    let manifest =
        format!(r#"{{"openapi":"3.1.0","info":{{"title":"HTTP","version":"1"}},"paths":{paths}}}"#);
    let mut section = Vec::new();
    leb(b"pishoo:openapi".len(), &mut section);
    section.extend_from_slice(b"pishoo:openapi");
    section.extend_from_slice(manifest.as_bytes());
    let mut bytes =
        include_bytes!("../../fixtures/wasi-http-stream-response-until-cancelled.wasm").to_vec();
    bytes.push(0);
    leb(section.len(), &mut bytes);
    bytes.extend(section);
    Arc::new(
        Lib::load(
            sandbox.runtime.clone(),
            "test".into(),
            &bytes,
            directory,
            LibPolicy::default(),
            cancel,
        )
        .unwrap(),
    )
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
async fn api_methods_require_manifest_entries_and_namespace_never_falls_through() {
    let sandbox = Sandbox::new(Arc::new(Runtime::new().unwrap()));
    let directory = tempfile::tempdir().unwrap();
    let lib = load(
        &sandbox,
        directory.path(),
        r#"{"/run":{"get":{},"post":{}},"/explicit":{"head":{},"options":{}}}"#,
        CancellationToken::new(),
    );
    let router = sandbox.api_router(
        dhttp::Endpoint::load("alice").await.unwrap(),
        &BTreeMap::from([("test".into(), lib)]),
    );
    for (method, path, expected) in [
        (
            Method::GET,
            "/api/test/run",
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        (
            Method::POST,
            "/api/test/run?value=1",
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        (
            Method::HEAD,
            "/api/test/run",
            StatusCode::METHOD_NOT_ALLOWED,
        ),
        (
            Method::OPTIONS,
            "/api/test/run",
            StatusCode::METHOD_NOT_ALLOWED,
        ),
        (
            Method::HEAD,
            "/api/test/explicit",
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        (
            Method::OPTIONS,
            "/api/test/explicit",
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    ] {
        assert_eq!(status(&router, method, path).await, expected, "{path}");
    }
    for path in ["/api", "/api/", "/api/missing", "/api/test/missing"] {
        for method in [Method::GET, Method::POST, Method::HEAD, Method::OPTIONS] {
            assert_eq!(status(&router, method, path).await, StatusCode::NOT_FOUND);
        }
    }
    assert!(sandbox.tasks.is_empty());
}

#[tokio::test]
async fn routers_keep_their_candidate_snapshot_and_cancel_deleted_versions() {
    let mut sandbox = Sandbox::new(Arc::new(Runtime::new().unwrap()));
    let directory = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let old = load(
        &sandbox,
        directory.path(),
        r#"{"/old":{"post":{}}}"#,
        cancel.clone(),
    );
    let candidate = BTreeMap::from([("test".into(), old)]);
    let endpoint = dhttp::Endpoint::load("alice").await.unwrap();
    let old_router = sandbox.api_router(endpoint.clone(), &candidate);
    sandbox.replace_libs(candidate);
    let new = load(
        &sandbox,
        directory.path(),
        r#"{"/new":{"post":{}}}"#,
        cancel.clone(),
    );
    let candidate = BTreeMap::from([("test".into(), new)]);
    let new_router = sandbox.api_router(endpoint, &candidate);
    sandbox.replace_libs(candidate);
    for (router, present, absent) in [
        (&old_router, "/api/test/old", "/api/test/new"),
        (&new_router, "/api/test/new", "/api/test/old"),
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
    assert!(!cancel.is_cancelled());
    sandbox.replace_libs(BTreeMap::new());
    assert!(cancel.is_cancelled());
    for (router, path) in [
        (&old_router, "/api/test/old"),
        (&new_router, "/api/test/new"),
    ] {
        assert_eq!(
            status(router, Method::POST, path).await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}

#[tokio::test]
async fn routed_executions_exceed_four_concurrent_requests_and_close_reaps_them() {
    let mut sandbox = Sandbox::new(Arc::new(Runtime::new().unwrap()));
    let directory = tempfile::tempdir().unwrap();
    let lib = load(
        &sandbox,
        directory.path(),
        r#"{"/run":{"post":{}}}"#,
        CancellationToken::new(),
    );
    sandbox.replace_libs(BTreeMap::from([("test".into(), lib)]));
    let router = sandbox.api_router(dhttp::Endpoint::load("alice").await.unwrap(), &sandbox.libs);
    assert_eq!(
        status(&router, Method::POST, "/api/test/run").await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert!(sandbox.tasks.is_empty());
    let name = "alice.dhttp.net";
    let certificate = rcgen::generate_simple_self_signed(vec![name.into()]).unwrap();
    let local = dhttp::LocalAuthority::new(
        &qtls::default_provider(),
        Arc::from(name),
        vec![certificate.cert.der().clone()],
        qtls::PrivateKeyDer::try_from(certificate.signing_key.serialize_der()).unwrap(),
        vec![1],
    )
    .unwrap();
    let mut responses = Vec::new();
    for index in 0..8 {
        let mut request = request(Method::POST, &format!("/api/test/run?index={index}"));
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
    sandbox.wait().await.unwrap();
    assert!(sandbox.tasks.is_empty());
    assert_eq!(
        status(&router, Method::POST, "/api/test/run").await,
        StatusCode::SERVICE_UNAVAILABLE
    );
    drop(responses);
}
