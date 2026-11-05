use std::io::Write;

use axum::response::Response;
use http::StatusCode;
use http_body_util::BodyExt;
use tower::ServiceExt;

use super::*;

fn environment() -> (tempfile::TempDir, IdentityProfile, Arc<RwLock<Sandbox>>) {
    environment_named("dev.test.sky.lee.dhttp.net")
}
fn environment_named(name: &str) -> (tempfile::TempDir, IdentityProfile, Arc<RwLock<Sandbox>>) {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join(name)).unwrap();
    std::fs::create_dir_all(profile.join("dev/hello/app")).unwrap();
    std::fs::write(
        profile.join("dev/hello/manifest.json"),
        serde_json::to_vec(&serde_json::json!({"package":{"author":name,"name":"hello"}})).unwrap(),
    )
    .unwrap();
    let sandbox = Arc::new(RwLock::new(Sandbox::new(Arc::new(
        crate::WasmRuntime::new().unwrap(),
    ))));
    (root, profile, sandbox)
}
fn app(profile: &IdentityProfile, sandbox: &Arc<RwLock<Sandbox>>) -> Router {
    let endpoint = crate::test_support::identity::endpoint(profile.name());
    router(
        profile.clone(),
        sandbox.clone(),
        endpoint,
        Arc::new(access_control::AccessService::new(
            sea_orm::DatabaseConnection::default(),
            profile.name(),
        )),
        Router::new(),
        Router::new(),
    )
}
fn request(path: &str, owner: bool) -> Request<Body> {
    request_named("dev.test.sky.lee.dhttp.net", path, owner)
}
fn request_named(name: &str, path: &str, owner: bool) -> Request<Body> {
    let endpoint = crate::test_support::identity::endpoint(name);
    let local = endpoint.local_authority().unwrap();
    let ski = dhttp_home::certificate::extract_dhttp_subject_key_identifier(local.certificates())
        .unwrap();
    let subject = access_control::SubjectId::new(ski.owner_hash().as_str().as_bytes()).unwrap();
    let mut request = Request::builder()
        .uri(format!("https://{name}{path}"))
        .body(Body::empty())
        .unwrap();
    if owner {
        request
            .extensions_mut()
            .insert(access_control::Visitor::new(endpoint.name(), subject));
    }
    request.extensions_mut().insert(dhttp::HandshakeSummary {
        alpn: None,
        local: Some(local),
        remote: None,
    });
    request
}
async fn text(response: Response) -> Vec<u8> {
    response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}
fn wasm(version: &str) -> Vec<u8> {
    fn leb(mut value: usize, output: &mut Vec<u8>) {
        loop {
            let byte = (value & 127) as u8;
            value >>= 7;
            output.push(byte | if value == 0 { 0 } else { 128 });
            if value == 0 {
                break;
            }
        }
    }
    let document=serde_json::to_vec(&serde_json::json!({"openapi":"3.1.0","info":{"title":"Development","version":version},"paths":{"/read":{"post":{"operationId":"read","summary":"Read request","responses":{"201":{"description":"Created"}}}}}})).unwrap();
    let mut section = Vec::new();
    leb(b"pishoo:openapi".len(), &mut section);
    section.extend_from_slice(b"pishoo:openapi");
    section.extend(document);
    let mut bytes = include_bytes!("../fixtures/wasi-http-read-request-then-respond.wasm").to_vec();
    bytes.push(0);
    leb(section.len(), &mut bytes);
    bytes.extend(section);
    bytes
}
fn replace(path: &std::path::Path, bytes: &[u8]) {
    let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap()).unwrap();
    file.write_all(bytes).unwrap();
    file.persist(path).unwrap();
}

#[tokio::test]
async fn local_app_ranges_conditions_redirects_and_errors_remain_no_store() {
    let (_temp, profile, sandbox) = environment();
    std::fs::write(profile.join("dev/hello/app/index.html"), b"0123456789").unwrap();
    let app = app(&profile, &sandbox);
    let initial = app
        .clone()
        .oneshot(request("/app/hello/", true))
        .await
        .unwrap();
    let etag = initial.headers()["etag"].clone();
    assert_eq!(initial.headers()["cache-control"], "no-store");
    let mut conditional = request("/app/hello/", true);
    conditional
        .headers_mut()
        .insert("if-none-match", etag.clone());
    let response = app.clone().oneshot(conditional).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let mut ranged = request("/app/hello/", true);
    ranged
        .headers_mut()
        .insert("range", "bytes=2-4".parse().unwrap());
    let response = app.clone().oneshot(ranged).await.unwrap();
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(text(response).await, b"234");
    for (path, owner, status) in [
        ("/app/hello", true, StatusCode::TEMPORARY_REDIRECT),
        ("/app/hello/assets/missing.js", true, StatusCode::NOT_FOUND),
        ("/app/hello/", false, StatusCode::FORBIDDEN),
    ] {
        let response = app.clone().oneshot(request(path, owner)).await.unwrap();
        assert_eq!(response.status(), status);
        assert_eq!(response.headers()["cache-control"], "no-store");
    }
    replace(&profile.join("dev/hello/app/index.html"), b"replacement");
    let mut conditional = request("/app/hello/", true);
    conditional.headers_mut().insert("if-none-match", etag);
    let response = app.oneshot(conditional).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(text(response).await, b"replacement");
}

#[tokio::test]
async fn files_are_discovered_without_registration_and_survive_router_restart() {
    let (_temp, profile, sandbox) = environment();
    std::fs::write(profile.join("dev/hello/app/index.html"), "hello").unwrap();
    for _ in 0..2 {
        let response = app(&profile, &sandbox)
            .oneshot(request("/app/hello/", true))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(text(response).await, b"hello");
    }
    assert!(sandbox.read().unwrap().apps.is_empty());
    assert!(sandbox.read().unwrap().libs.is_empty());
    assert!(!profile.join(".volume/sys/pkg/database").exists());
    assert_eq!(
        app(&profile, &sandbox)
            .oneshot(request("/dev/app/hello/", true))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn directory_replacement_and_removal_affect_new_requests_only() {
    let (_temp, profile, sandbox) = environment();
    std::fs::write(profile.join("dev/hello/app/index.html"), "old").unwrap();
    let api = app(&profile, &sandbox);
    let response = api
        .clone()
        .oneshot(request("/app/hello/", true))
        .await
        .unwrap();
    std::fs::rename(profile.join("dev/hello"), profile.join("dev/.previous")).unwrap();
    std::fs::create_dir_all(profile.join("dev/hello/app")).unwrap();
    std::fs::write(profile.join("dev/hello/app/index.html"), "new").unwrap();
    std::fs::write(
        profile.join("dev/hello/manifest.json"),
        r#"{"package":{"author":"dev.test.sky.lee.dhttp.net","name":"hello"}}"#,
    )
    .unwrap();
    std::fs::remove_dir_all(profile.join("dev/.previous")).unwrap();
    assert_eq!(text(response).await, b"old");
    assert_eq!(
        text(
            api.clone()
                .oneshot(request("/app/hello/", true))
                .await
                .unwrap()
        )
        .await,
        b"new"
    );
    std::fs::remove_dir_all(profile.join("dev/hello")).unwrap();
    assert_eq!(
        api.oneshot(request("/app/hello/", true))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn development_files_require_owner_and_reject_path_escape() {
    let (_temp, profile, sandbox) = environment();
    std::fs::write(profile.join("dev/hello/app/index.html"), "private").unwrap();
    let api = app(&profile, &sandbox);
    for path in ["/app/hello/", "/hello/read"] {
        assert_eq!(
            api.clone()
                .oneshot(request(path, false))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    for path in [
        "/app/hello/%2e%2e/lib.wasm",
        "/app/hello/%252e%252e/secret",
        "/app/hello/assets/missing.js",
        "/%2e%2e/read",
    ] {
        assert_eq!(
            api.clone()
                .oneshot(request(path, true))
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
    std::os::unix::fs::symlink(profile.path(), profile.join("dev/hello/app/escape")).unwrap();
    assert_eq!(
        api.clone()
            .oneshot(request("/app/hello/escape/file.txt", true))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    std::fs::remove_dir_all(profile.join("dev/hello/app")).unwrap();
    std::os::unix::fs::symlink(profile.path(), profile.join("dev/hello/app")).unwrap();
    assert_eq!(
        api.oneshot(request("/app/hello/", true))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn development_frontend_preserves_redirect_head_and_spa_behavior() {
    let (_temp, profile, sandbox) = environment();
    std::fs::write(profile.join("dev/hello/app/index.html"), "page").unwrap();
    let api = app(&profile, &sandbox);
    let response = api
        .clone()
        .oneshot(request("/app/hello?x=1", true))
        .await
        .unwrap();
    assert_eq!(
        response.headers()[http::header::LOCATION],
        "/app/hello/?x=1"
    );
    let mut head = request("/app/hello/", true);
    *head.method_mut() = http::Method::HEAD;
    let response = api.clone().oneshot(head).await.unwrap();
    assert_eq!(response.headers()[http::header::CONTENT_LENGTH], "4");
    assert!(text(response).await.is_empty());
    assert_eq!(
        text(
            api.clone()
                .oneshot(request("/app/hello/settings", true))
                .await
                .unwrap()
        )
        .await,
        b"page"
    );
    let mut post = request("/app/hello/", true);
    *post.method_mut() = http::Method::POST;
    assert_eq!(
        api.oneshot(post).await.unwrap().status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
}

#[test]
fn wasm_cache_follows_files_and_keeps_test_data_separate() {
    let (_temp, profile, sandbox) = environment();
    let path = profile.join("dev/hello/lib.wasm");
    replace(&path, &wasm("1.0.0"));
    let first = load(&profile, &sandbox, "dev.test.sky.lee.dhttp.net", "hello").unwrap();
    assert!(Arc::ptr_eq(
        &first,
        &load(&profile, &sandbox, "dev.test.sky.lee.dhttp.net", "hello").unwrap()
    ));
    std::fs::write(profile.join(".volume/hello/database"), "persistent data").unwrap();
    replace(&path, &wasm("1.0.1"));
    let second = load(&profile, &sandbox, "dev.test.sky.lee.dhttp.net", "hello").unwrap();
    assert!(!Arc::ptr_eq(&first, &second));
    assert_eq!(
        std::fs::read(profile.join(".volume/hello/database")).unwrap(),
        b"persistent data"
    );
    assert!(sandbox.read().unwrap().libs.is_empty());
    replace(&path, b"invalid WASM");
    assert!(load(&profile, &sandbox, "dev.test.sky.lee.dhttp.net", "hello").is_err());
    assert!(!sandbox.read().unwrap().dev_libs.contains_key("hello"));
    replace(&path, &wasm("1.0.2"));
    load(&profile, &sandbox, "dev.test.sky.lee.dhttp.net", "hello").unwrap();
    std::fs::remove_file(&path).unwrap();
    assert!(load(&profile, &sandbox, "dev.test.sky.lee.dhttp.net", "hello").is_err());
    assert!(sandbox.read().unwrap().dev_libs.is_empty());
    assert!(profile.join(".volume/hello/database").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn development_api_executes_wasm_with_prefix_removed_and_streaming_body_preserved() {
    let (_temp, profile, sandbox) = environment();
    replace(&profile.join("dev/hello/lib.wasm"), &wasm("1.0.0"));
    let mut request = request("/hello/read", true);
    *request.method_mut() = http::Method::POST;
    let mut trailers = http::HeaderMap::new();
    trailers.append("x-request-trailer", "preserved".parse().unwrap());
    *request.body_mut() = Body::new(http_body_util::StreamBody::new(futures::stream::iter(
        vec![
            Ok::<_, std::io::Error>(http_body::Frame::data(bytes::Bytes::from_static(
                b"hello-hello-hello-hello-",
            ))),
            Ok(http_body::Frame::data(bytes::Bytes::from_static(
                b"hello-hello-hello-hello",
            ))),
            Ok(http_body::Frame::trailers(trailers)),
        ],
    )));
    let response = app(&profile, &sandbox).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(text(response).await, b"world-world-".repeat(64));
}

#[ignore = "requires JADE_TEST_BINARY built from the matching Jade checkout"]
#[tokio::test(flavor = "multi_thread")]
async fn jade_publishes_without_a_control_connection_and_cleans_up_on_stop() {
    let binary = std::env::var_os("JADE_TEST_BINARY").expect("set JADE_TEST_BINARY");
    let root = tempfile::tempdir().unwrap();
    let home = dhttp_home::DhttpHome::new(root.path().join("home"));
    let profile = home.identity_profile("dev.test.sky.lee.dhttp.net").unwrap();
    std::fs::create_dir_all(profile.path()).unwrap();
    let project = root.path().join("project");
    std::fs::create_dir_all(project.join("public")).unwrap();
    std::fs::write(
        project.join("jade.toml"),
        "[package]\nname='hello'\nversion='1.0.0'\ndescription='local'\nlicense='MIT'\n[[dependencies]]\nauthor='std.dhttp.net'\nname='profile'\nversion='0.3.0'\npath='../profile'\n",
    )
    .unwrap();
    std::fs::write(project.join("public/index.html"), "without socket").unwrap();
    std::fs::write(project.join("lib.wasm"), wasm("1.0.0")).unwrap();
    let dependency = root.path().join("profile");
    std::fs::create_dir_all(dependency.join("public")).unwrap();
    std::fs::write(dependency.join("jade.toml"), "[package]\nname='profile'\nauthor='std.dhttp.net'\nversion='0.3.0'\ndescription='local dependency'\nlicense='MIT'\n").unwrap();
    std::fs::write(dependency.join("public/index.html"), "dependency v1").unwrap();
    let host_binary = std::env::var_os("PISHOO_TEST_BINARY").expect("set PISHOO_TEST_BINARY");
    let mut child = tokio::process::Command::new(binary)
        .args(["run", "--id", "dev.test.sky.lee", "--json"])
        .arg(&project)
        .env("DHTTP_HOME", root.path().join("home"))
        .env("PISHOO_BIN", host_binary)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let ready = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        while !profile.join("dev/hello/app/index.html").exists() {
            if let Some(status) = child.try_wait().unwrap() {
                panic!("jade exited before publication: {status}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    if ready.is_err() {
        child.start_kill().unwrap();
        let output = child.wait_with_output().await.unwrap();
        panic!(
            "Jade did not publish: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    assert!(profile.join("dev/hello/lib.wasm").exists());
    assert!(!profile.ssl_dir().exists());
    let sandbox = Arc::new(RwLock::new(Sandbox::new(Arc::new(
        crate::WasmRuntime::new().unwrap(),
    ))));
    load(&profile, &sandbox, "dev.test.sky.lee.dhttp.net", "hello").unwrap();
    let api = app(&profile, &sandbox);
    assert_eq!(
        text(
            api.clone()
                .oneshot(request("/app/hello/", true))
                .await
                .unwrap()
        )
        .await,
        b"without socket"
    );
    std::fs::write(profile.join(".volume/hello/database"), "keep").unwrap();
    let dependency_page = profile.join("dev/profile/app/index.html");
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while std::fs::read(&dependency_page).ok().as_deref() != Some(b"dependency v1") {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    std::fs::write(
        profile.join(".volume/std/profile/database"),
        "dependency data",
    )
    .unwrap();
    std::fs::write(dependency.join("public/index.html"), "dependency v2").unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while std::fs::read(&dependency_page).ok().as_deref() != Some(b"dependency v2") {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let status = std::process::Command::new("kill")
        .args(["-INT", &child.id().unwrap().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    let output = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!profile.join("dev/hello").exists());
    assert!(!profile.join("dev/profile").exists());
    assert_eq!(
        std::fs::read(profile.join(".volume/std/profile/database")).unwrap(),
        b"dependency data"
    );
    assert_eq!(
        std::fs::read(profile.join(".volume/hello/database")).unwrap(),
        b"keep"
    );
    assert_eq!(
        api.clone()
            .oneshot(request("/app/hello/", true))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        api.oneshot(request("/hello/read", true))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert!(sandbox.read().unwrap().dev_libs.is_empty());
}

#[tokio::test]
async fn local_files_require_the_real_owner_independently_of_identity_spelling() {
    for name in [
        "alex.chen.dhttp.net",
        "dev.wang.dhttp.net",
        "dev.dev.wang.dhttp.net",
    ] {
        let (_temp, profile, sandbox) = environment_named(name);
        std::fs::write(profile.join("dev/hello/app/index.html"), "private").unwrap();
        let routes = app(&profile, &sandbox);
        let response = routes
            .clone()
            .oneshot(request_named(name, "/app/hello/", true))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{name}");
        assert_eq!(text(response).await, b"private");
        assert_eq!(
            routes
                .oneshot(request_named(name, "/app/hello/", false))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN,
            "{name}"
        );
        assert!(sandbox.read().unwrap().dev_libs.is_empty());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn standard_local_library_uses_normal_namespace_and_private_volume() {
    let (_temp, profile, sandbox) = environment();
    std::fs::write(
        profile.join("dev/hello/manifest.json"),
        r#"{"package":{"author":"std.dhttp.net","name":"hello"}}"#,
    )
    .unwrap();
    replace(&profile.join("dev/hello/lib.wasm"), &wasm("1.0.0"));
    let lib = load(&profile, &sandbox, "std.dhttp.net", "hello").unwrap();
    assert!(Arc::ptr_eq(
        &lib,
        &sandbox.read().unwrap().dev_libs["std.dhttp.net/hello"].1
    ));
    assert!(profile.join(".volume/std/hello").is_dir());
    assert!(!profile.join(".volume/hello").exists());
    // A standard library never gains the ordinary short-name route.
    assert_eq!(
        app(&profile, &sandbox)
            .oneshot(request("/hello/read", true))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let mut call = request("/std/hello/read?example=1", true);
    *call.method_mut() = http::Method::POST;
    let mut trailers = http::HeaderMap::new();
    trailers.append("x-request-trailer", "preserved".parse().unwrap());
    *call.body_mut() = Body::new(http_body_util::StreamBody::new(futures::stream::iter(
        vec![
            Ok::<_, std::io::Error>(http_body::Frame::data(bytes::Bytes::from_static(
                b"hello-hello-hello-hello-hello-hello-hello-hello",
            ))),
            Ok(http_body::Frame::trailers(trailers)),
        ],
    )));
    let response = app(&profile, &sandbox).oneshot(call).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(text(response).await, b"world-world-".repeat(64));
}

#[tokio::test]
async fn installed_dependency_and_native_routes_remain_available_with_local_files() {
    let (_temp, profile, sandbox) = environment();
    let endpoint = crate::test_support::identity::endpoint("dev.test.sky.lee.dhttp.net");
    let routes = router(
        profile,
        sandbox,
        endpoint,
        Arc::new(access_control::AccessService::new(
            sea_orm::DatabaseConnection::default(),
            "dev.test.sky.lee.dhttp.net",
        )),
        Router::new(),
        Router::new()
            .route(
                "/std/message/list",
                axum::routing::get(|| async { "installed dependency" }),
            )
            .route("/std/sys/listen", axum::routing::get(|| async { "native" })),
    );
    for (path, expected) in [
        ("/std/message/list", "installed dependency"),
        ("/std/sys/listen", "native"),
    ] {
        let response = routes.clone().oneshot(request(path, true)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(text(response).await, expected.as_bytes());
    }
}
