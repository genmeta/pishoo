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
    std::fs::create_dir_all(profile.join("app/hello")).unwrap();
    std::fs::create_dir_all(profile.join("lib/hello.preview")).unwrap();
    std::fs::write(
        profile.join("lib/hello.preview/manifest.json"),
        serde_json::to_vec(
            &serde_json::json!({"package":{"author":name,"name":"hello","version":"1.0.0"}}),
        )
        .unwrap(),
    )
    .unwrap();
    let sandbox = Arc::new(RwLock::new(Sandbox::new(Arc::new(
        crate::WasmRuntime::new().unwrap(),
    ))));
    (root, profile, sandbox)
}
fn app(profile: &IdentityProfile, sandbox: &Arc<RwLock<Sandbox>>) -> Router {
    let endpoint = crate::test_support::identity::endpoint(profile.name());
    let access = Arc::new(access_control::AccessService::new(
        sea_orm::DatabaseConnection::default(),
        profile.name(),
    ));
    let fallback = Sandbox::api_router(
        sandbox.clone(),
        endpoint.clone(),
        access.clone(),
        Router::new(),
    );
    router(
        profile.clone(),
        sandbox.clone(),
        endpoint,
        access,
        Router::new(),
        fallback,
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
    wasm_with_path(version, "/read")
}

fn wasm_with_path(version: &str, path: &str) -> Vec<u8> {
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
    let document=serde_json::to_vec(&serde_json::json!({"openapi":"3.1.0","info":{"title":"Development","version":version},"paths":{path:{"post":{"operationId":"read","summary":"Read request","responses":{"201":{"description":"Created"}}}}}})).unwrap();
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

fn standard_files(profile: &IdentityProfile, name: &str, path: &str) -> std::path::PathBuf {
    let directory = profile.join("lib").join(format!("{name}.preview"));
    std::fs::create_dir_all(profile.join("lib/std").join(name).parent().unwrap()).unwrap();
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("manifest.json"),
        serde_json::to_vec(&serde_json::json!({
            "package": {"author": "dev.genmeta.tech.dhttp.net", "name": name, "version":"1.0.0"}
        }))
        .unwrap(),
    )
    .unwrap();
    let file = profile.join("lib/std").join(format!("{name}.wasm.1.0.0"));
    replace(&file, &wasm_with_path("1.0.0", path));
    file
}

#[tokio::test]
async fn local_app_ranges_conditions_redirects_and_errors_remain_no_store() {
    let (_temp, profile, sandbox) = environment();
    std::fs::write(profile.join("app/hello/index.html"), b"0123456789").unwrap();
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
    replace(&profile.join("app/hello/index.html"), b"replacement");
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
    std::fs::write(profile.join("app/hello/index.html"), "hello").unwrap();
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
    std::fs::write(profile.join("app/hello/index.html"), "old").unwrap();
    let api = app(&profile, &sandbox);
    let response = api
        .clone()
        .oneshot(request("/app/hello/", true))
        .await
        .unwrap();
    std::fs::rename(
        profile.join("lib/hello.preview"),
        profile.join("lib/.previous"),
    )
    .unwrap();
    std::fs::rename(profile.join("app/hello"), profile.join("app/.previous")).unwrap();
    std::fs::create_dir_all(profile.join("app/hello")).unwrap();
    std::fs::create_dir_all(profile.join("lib/hello.preview")).unwrap();
    std::fs::write(profile.join("app/hello/index.html"), "new").unwrap();
    std::fs::write(
        profile.join("lib/hello.preview/manifest.json"),
        r#"{"package":{"author":"dev.test.sky.lee.dhttp.net","name":"hello","version":"1.0.0"}}"#,
    )
    .unwrap();
    std::fs::remove_dir_all(profile.join("lib/.previous")).unwrap();
    std::fs::remove_dir_all(profile.join("app/.previous")).unwrap();
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
    std::fs::remove_dir_all(profile.join("lib/hello.preview")).unwrap();
    std::fs::remove_dir_all(profile.join("app/hello")).unwrap();
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
    std::fs::write(profile.join("app/hello/index.html"), "private").unwrap();
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
    std::os::unix::fs::symlink(profile.path(), profile.join("app/hello/escape")).unwrap();
    assert_eq!(
        api.clone()
            .oneshot(request("/app/hello/escape/file.txt", true))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    std::fs::remove_dir_all(profile.join("app/hello")).unwrap();
    std::os::unix::fs::symlink(profile.path(), profile.join("app/hello")).unwrap();
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
    std::fs::write(profile.join("app/hello/index.html"), "page").unwrap();
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
    let path = profile.join("lib/hello.wasm.1.0.0");
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
    replace(&profile.join("lib/hello.wasm.1.0.0"), &wasm("1.0.0"));
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
    crate::test_support::credentials::generate_with_owner(
        &root.path().join("home"),
        &[(
            profile.path().file_name().unwrap().to_str().unwrap(),
            profile.name(),
        )],
        Some((profile.name(), &"a".repeat(64))),
    );
    let project = root.path().join("project");
    std::fs::create_dir_all(project.join("public")).unwrap();
    std::fs::write(
        project.join("jade.toml"),
        "[package]\nname='hello'\nauthor='dev.test.sky.lee'\nversion='1.0.0'\ndescription='local'\nlicense='MIT'\n[[dependencies]]\nname='profile'\nversion='0.3.0'\npath='../profile'\n",
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
        .args(["run", "--json"])
        .arg(&project)
        .env("DHTTP_HOME", root.path().join("home"))
        .env("PISHOO_BIN", host_binary)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let ready = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        while !profile.join("app/hello/index.html").exists() {
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

    assert!(profile.join("lib/hello.wasm.1.0.0").exists());
    assert!(profile.ssl_dir().exists());
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
    let dependency_page = profile.join("app/profile/index.html");
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
    assert!(!profile.join("lib/hello.preview").exists());
    assert!(!profile.join("lib/profile.preview").exists());
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
        std::fs::write(profile.join("app/hello/index.html"), "private").unwrap();
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
        profile.join("lib/hello.preview/manifest.json"),
        r#"{"package":{"author":"std.dhttp.net","name":"hello","version":"1.0.0"}}"#,
    )
    .unwrap();
    std::fs::create_dir_all(profile.join("lib/std")).unwrap();
    replace(&profile.join("lib/std/hello.wasm.1.0.0"), &wasm("1.0.0"));
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
    assert_eq!(
        app(&profile, &sandbox)
            .oneshot(request("/std/hello/read", true))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let mut call = request("/std/read?example=1", true);
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
    standard_files(&profile, "conversations", "/comm/group");
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
                "/std/comm/message/list",
                axum::routing::get(|| async { "installed dependency" }),
            )
            .route("/std/sys/listen", axum::routing::get(|| async { "native" })),
    );
    for (path, expected) in [
        ("/std/comm/message/list", "installed dependency"),
        ("/std/sys/listen", "native"),
    ] {
        let response = routes.clone().oneshot(request(path, true)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(text(response).await, expected.as_bytes());
    }
}

async fn status(routes: &Router, path: &str, owner: bool) -> StatusCode {
    routes
        .clone()
        .oneshot(request(path, owner))
        .await
        .unwrap()
        .status()
}

fn component_request(path: &str) -> Request<Body> {
    let mut call = request(path, true);
    *call.method_mut() = http::Method::POST;
    let mut trailers = http::HeaderMap::new();
    trailers.insert("x-request-trailer", "preserved".parse().unwrap());
    *call.body_mut() = Body::new(http_body_util::StreamBody::new(futures::stream::iter([
        Ok::<_, std::io::Error>(http_body::Frame::data(bytes::Bytes::from_static(
            b"hello-hello-hello-hello-hello-hello-hello-hello",
        ))),
        Ok(http_body::Frame::trailers(trailers)),
    ])));
    call
}

#[tokio::test(flavor = "multi_thread")]
async fn standard_declarations_select_arbitrary_package_names_and_require_owner() {
    let (_temp, profile, sandbox) = environment();
    standard_files(&profile, "comm", "/rooms/send");
    let routes = app(&profile, &sandbox);
    assert_eq!(
        status(&routes, "/std/rooms/send", false).await,
        StatusCode::FORBIDDEN
    );
    let call = component_request("/std/rooms/send?value=a%2Fb");
    let response = routes.clone().oneshot(call).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(text(response).await, b"world-world-".repeat(64));
    assert!(profile.join(".volume/std/comm").is_dir());
    assert_eq!(
        status(&routes, "/std/rooms/send", true).await,
        StatusCode::METHOD_NOT_ALLOWED
    );
    for path in [
        "/std/comm/rooms/send",
        "/std/rooms/send/extra",
        "/comm/rooms/send",
    ] {
        assert_eq!(
            status(&routes, path, true).await,
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
}

#[tokio::test]
async fn standard_preview_path_changes_retire_code_and_removal_restores_installed_routes() {
    let (_temp, profile, sandbox) = environment();
    let file = standard_files(&profile, "conversations", "/comm/group");
    let id = "dev.genmeta.tech.dhttp.net/conversations";
    let data = tempfile::tempdir().unwrap();
    let installed = Arc::new(
        Lib::load(
            sandbox.read().unwrap().runtime.clone(),
            id.to_owned(),
            &wasm_with_path("0.9.0", "/comm/installed"),
            data.path(),
        )
        .unwrap(),
    );
    sandbox.write().unwrap().libs.insert(id.into(), installed);
    let routes = app(&profile, &sandbox);
    assert_eq!(
        status(&routes, "/std/comm/group", true).await,
        StatusCode::METHOD_NOT_ALLOWED
    );
    let first = sandbox.read().unwrap().dev_libs[id].1.clone();
    assert_eq!(
        status(&routes, "/std/comm/installed", true).await,
        StatusCode::NOT_FOUND
    );
    replace(&file, &wasm_with_path("1.0.1", "/rooms/read"));
    assert_eq!(
        status(&routes, "/std/comm/group", true).await,
        StatusCode::NOT_FOUND
    );
    assert!(first.stream_retired.is_cancelled());
    assert_eq!(
        status(&routes, "/std/rooms/read", true).await,
        StatusCode::METHOD_NOT_ALLOWED
    );
    let second = sandbox.read().unwrap().dev_libs[id].1.clone();
    assert!(!Arc::ptr_eq(&first, &second));
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(profile.path(), std::fs::Permissions::from_mode(0o775)).unwrap();
    assert_eq!(
        status(&routes, "/std/rooms/read", true).await,
        StatusCode::FORBIDDEN
    );
    std::fs::remove_dir_all(profile.join("lib/conversations.preview")).unwrap();
    std::fs::remove_dir_all(profile.join("lib/hello.preview")).unwrap();
    for mode in [0o775, 0o755] {
        std::fs::set_permissions(profile.path(), std::fs::Permissions::from_mode(mode)).unwrap();
        let response = routes
            .clone()
            .oneshot(component_request("/std/comm/installed"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(text(response).await, b"world-world-".repeat(64));
    }
    assert!(second.stream_retired.is_cancelled());
    assert!(sandbox.read().unwrap().dev_libs.is_empty());
    assert_eq!(
        status(&routes, "/std/rooms/read", true).await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn standard_preview_rejects_duplicate_providers_and_keeps_native_precedence() {
    let (_temp, profile, sandbox) = environment();
    standard_files(&profile, "one", "/comm/group");
    standard_files(&profile, "two", "/comm/group");
    assert_eq!(
        app(&profile, &sandbox)
            .oneshot(request("/std/comm/group", true))
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    std::fs::remove_dir_all(profile.join("lib/two.preview")).unwrap();
    let data = tempfile::tempdir().unwrap();
    let installed = Arc::new(
        Lib::load(
            sandbox.read().unwrap().runtime.clone(),
            "std.dhttp.net/another".into(),
            &wasm_with_path("1.0.0", "/comm/group"),
            data.path(),
        )
        .unwrap(),
    );
    sandbox
        .write()
        .unwrap()
        .libs
        .insert("std.dhttp.net/another".into(), installed);
    assert_eq!(
        app(&profile, &sandbox)
            .oneshot(request("/std/comm/group", true))
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    standard_files(&profile, "one", "/sys/listen");
    let endpoint = crate::test_support::identity::endpoint(profile.name());
    let routes = router(
        profile,
        sandbox,
        endpoint,
        Arc::new(access_control::AccessService::new(
            sea_orm::DatabaseConnection::default(),
            "dev.test.sky.lee.dhttp.net",
        )),
        Router::new(),
        Router::new().route("/std/sys/listen", axum::routing::get(|| async { "native" })),
    );
    let response = routes
        .oneshot(request("/std/sys/listen", true))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(text(response).await, b"native");
}

#[tokio::test]
async fn hierarchical_standard_preview_uses_complete_identity_and_isolated_volume() {
    let (_temp, profile, sandbox) = environment();
    let name = "comm/group";
    let directory = profile.join("lib/comm/group.preview");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("manifest.json"),
        serde_json::to_vec(
            &serde_json::json!({"package":{"author":"dev.genmeta.tech.dhttp.net","name":name,"version":"1.0.0"}}),
        )
        .unwrap(),
    )
    .unwrap();
    std::fs::create_dir_all(profile.join("lib/std/comm")).unwrap();
    std::fs::write(
        profile.join("lib/std/comm/group.wasm.1.0.0"),
        wasm_with_path("1.0.0", "/comm/group"),
    )
    .unwrap();
    load_standard_libraries(&profile, &sandbox).unwrap();
    let routes = app(&profile, &sandbox);
    let mut input = request("/std/comm/group?probe=ok", true);
    *input.method_mut() = http::Method::POST;
    let mut trailers = http::HeaderMap::new();
    trailers.append("x-request-trailer", "preserved".parse().unwrap());
    *input.body_mut() = Body::new(http_body_util::StreamBody::new(futures::stream::iter(
        vec![
            Ok::<_, std::io::Error>(http_body::Frame::data(bytes::Bytes::from_static(
                b"hello-hello-hello-hello-hello-hello-hello-hello",
            ))),
            Ok(http_body::Frame::trailers(trailers)),
        ],
    )));
    let response = routes.oneshot(input).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    text(response).await;
    assert!(
        sandbox
            .read()
            .unwrap()
            .dev_libs
            .contains_key("dev.genmeta.tech.dhttp.net/comm/group")
    );
    assert!(profile.join(".volume/std/comm/group").is_dir());
    assert!(!profile.join(".volume/std/comm%2Fgroup").exists());
}

#[test]
fn hierarchical_preview_migrates_encoded_business_data_to_the_complete_path() {
    let (_temp, profile, sandbox) = environment();
    let directory = profile.join("lib/comm/group.preview");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("manifest.json"),
        serde_json::to_vec(
            &serde_json::json!({"package":{"author":profile.name(),"name":"comm/group","version":"1.0.0"}}),
        )
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        profile.join("lib/comm/group.wasm.1.0.0"),
        wasm_with_path("1.0.0", "/comm/group"),
    )
    .unwrap();
    std::fs::create_dir_all(profile.join(".volume/comm%2Fgroup")).unwrap();
    std::fs::write(
        profile.join(".volume/comm%2Fgroup/database"),
        b"existing data",
    )
    .unwrap();
    load(&profile, &sandbox, profile.name(), "comm/group").unwrap();
    assert_eq!(
        std::fs::read(profile.join(".volume/comm/group/database")).unwrap(),
        b"existing data"
    );
    assert!(!profile.join(".volume/comm%2Fgroup").exists());
}

#[tokio::test]
async fn frontend_fallback_keeps_a_backend_only_preview_running() {
    let (_temp, profile, sandbox) = environment();
    let path = profile.join("lib/hello.wasm.1.0.0");
    replace(&path, &wasm("1.0.0"));
    let metadata = path.metadata().unwrap();
    std::fs::write(
        profile.join("lib/hello.preview/state.json"),
        serde_json::to_vec(&serde_json::json!({
            "lib":{"dev":metadata.dev(),"ino":metadata.ino()}
        }))
        .unwrap(),
    )
    .unwrap();
    let lib = load(&profile, &sandbox, profile.name(), "hello").unwrap();
    assert_eq!(
        app(&profile, &sandbox)
            .oneshot(request("/app/hello/", true))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert!(!lib.stream_retired.is_cancelled());
    assert!(
        sandbox
            .read()
            .unwrap()
            .dev_libs
            .contains_key(&format!("{}/hello", profile.name()))
    );
}

#[test]
fn local_runtime_dependencies_allow_cycles_and_release_all_instances() {
    let (_root, profile, sandbox) = environment();
    for (name, dependency) in [("hello", "peer"), ("peer", "hello")] {
        std::fs::create_dir_all(profile.join(format!("lib/{name}.preview"))).unwrap();
        std::fs::write(
            profile.join(format!("lib/{name}.preview/manifest.json")),
            serde_json::to_vec(&serde_json::json!({
                "package":{"author":profile.name(),"name":name,"version":"1.0.0"},
                "dependencies":[{"name":dependency,"version":"1.0.0"}]
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            profile.join(format!("lib/{name}.wasm.1.0.0")),
            wasm("1.0.0"),
        )
        .unwrap();
    }
    load(&profile, &sandbox, profile.name(), "hello").unwrap();
    let state = sandbox.read().unwrap();
    assert_eq!(state.dev_libs.len(), 2);
    let hello = &state.dev_libs[&format!("{}/hello", profile.name())].1;
    let peer = &state.dev_libs[&format!("{}/peer", profile.name())].1;
    assert_eq!(
        hello.dependencies["peer"],
        (peer.id.clone(), "1.0.0".into())
    );
    assert_eq!(
        peer.dependencies["hello"],
        (hello.id.clone(), "1.0.0".into())
    );
    let weak = Arc::downgrade(hello);
    drop(state);
    drop(sandbox);
    assert!(weak.upgrade().is_none());
}
