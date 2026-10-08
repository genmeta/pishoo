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
    Arc::new(Lib::load(sandbox.runtime.clone(), "test".into(), &bytes, directory).unwrap())
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
async fn workspace_lib_catalog_lists_loaded_snapshot_and_requires_owner() {
    use access_control::{SubjectId, Visitor};

    let name = "alice.dhttp.net";
    let subject = "a".repeat(64);
    let mut params = rcgen::CertificateParams::new(vec![name.to_owned()]).unwrap();
    let ski = format!("0:0:{subject}");
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
    let endpoint = dhttp::Endpoint::new(
        qbase::endpoint::Endpoint::new(
            &qtls::default_provider(),
            name,
            vec![cert.der().clone()],
            qtls::PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
            vec![1],
        )
        .unwrap(),
    );
    let owner = Visitor::new(name, SubjectId::new(subject.as_bytes()).unwrap());
    let mut sandbox = Sandbox::new(Arc::new(WasmRuntime::new().unwrap()));
    let empty_router = sandbox.api_router(endpoint.clone());
    let directory = tempfile::tempdir().unwrap();
    let lib = load(
        &sandbox,
        directory.path(),
        r#"{
            "/": {
                "get": {"responses":{"200":{"description":"Note page"},"400":{"description":"Bad input"}}},
                "post": {"responses":{"201":{"description":"Created"},"400":{"description":"Bad input"}}}
            },
            "/run": {
                "get": {"summary":"Read run","description":"Ignored"},
                "post": {"description":"Create run"},
                "options": {"responses":{"200":{"$ref":"https://example.com/response"}}}
            }
        }"#,
    );
    sandbox.libs.insert("z-demo".into(), lib.clone());
    sandbox.libs.insert("a-demo".into(), lib);
    let router = sandbox.api_router(endpoint);
    sandbox.close();

    for (app, expected) in [
        (&empty_router, serde_json::json!([])),
        (
            &router,
            serde_json::json!([
                {"id":"a-demo","title":"HTTP","version":"1","description":null,"endpoints":[
                    {"method":"GET","path":"/api/a-demo/","description":"Note page"},
                    {"method":"POST","path":"/api/a-demo/","description":"Created"},
                    {"method":"GET","path":"/api/a-demo/run","description":"Read run"},
                    {"method":"OPTIONS","path":"/api/a-demo/run","description":null},
                    {"method":"POST","path":"/api/a-demo/run","description":"Create run"}
                ]},
                {"id":"z-demo","title":"HTTP","version":"1","description":null,"endpoints":[
                    {"method":"GET","path":"/api/z-demo/","description":"Note page"},
                    {"method":"POST","path":"/api/z-demo/","description":"Created"},
                    {"method":"GET","path":"/api/z-demo/run","description":"Read run"},
                    {"method":"OPTIONS","path":"/api/z-demo/run","description":null},
                    {"method":"POST","path":"/api/z-demo/run","description":"Create run"}
                ]}
            ]),
        ),
    ] {
        let mut request = request(Method::GET, "/workspace-api/libs");
        request.extensions_mut().insert(owner.clone());
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["cache-control"], "no-store");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            expected
        );
    }
    for visitor in [
        None,
        Some(Visitor::new(
            "bob.dhttp.net",
            SubjectId::new(subject.as_bytes()).unwrap(),
        )),
        Some(Visitor::new(
            name,
            SubjectId::new("b".repeat(64).as_bytes()).unwrap(),
        )),
    ] {
        let mut request = request(Method::GET, "/workspace-api/libs");
        if let Some(visitor) = visitor {
            request.extensions_mut().insert(visitor);
        }
        assert_eq!(
            router.clone().oneshot(request).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        status(&router, Method::POST, "/workspace-api/libs").await,
        StatusCode::METHOD_NOT_ALLOWED
    );
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
    let router = sandbox.api_router(crate::test_identity::endpoint("alice"));
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
async fn routers_keep_their_lib_snapshot() {
    let mut sandbox = Sandbox::new(Arc::new(WasmRuntime::new().unwrap()));
    let directory = tempfile::tempdir().unwrap();
    let old = load(&sandbox, directory.path(), r#"{"/old":{"post":{}}}"#);
    sandbox.libs.insert("test".into(), old);
    let endpoint = crate::test_identity::endpoint("alice");
    let old_router = sandbox.api_router(endpoint.clone());
    let new = load(&sandbox, directory.path(), r#"{"/new":{"post":{}}}"#);
    sandbox.libs.insert("test".into(), new);
    let new_router = sandbox.api_router(endpoint);
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
    sandbox.close();
    assert!(sandbox.libs.is_empty());
}

#[tokio::test]
async fn routed_executions_exceed_four_concurrent_requests_and_survive_close() {
    let mut sandbox = Sandbox::new(Arc::new(WasmRuntime::new().unwrap()));
    let directory = tempfile::tempdir().unwrap();
    let lib = load(&sandbox, directory.path(), r#"{"/run":{"post":{}}}"#);
    sandbox.libs.insert("test".into(), lib);
    let router = sandbox.api_router(crate::test_identity::endpoint("alice"));
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
    assert_eq!(sandbox.tasks.len(), 8);
    drop(responses);
}
