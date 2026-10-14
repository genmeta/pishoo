use super::*;
use crate::{Error, setup::load_server_config};

#[tokio::test]
async fn system_static_files_are_separate_from_ordinary_proxy_paths() {
    use http_body_util::BodyExt;
    use hyper::{body::Incoming, service::service_fn};
    use hyper_util::rt::TokioIo;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let upstream = tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        TokioIo::new(socket),
                        service_fn(|request: Request<Incoming>| async move {
                            Ok::<_, std::convert::Infallible>(http::Response::new(
                                http_body_util::Full::new(bytes::Bytes::from(format!(
                                    "upstream {}",
                                    request.uri()
                                ))),
                            ))
                        }),
                    )
                    .await
                    .unwrap();
            });
        }
    });
    let root = tempfile::tempdir().unwrap();
    let mut server = server(root.path()).await;
    std::fs::create_dir_all(server.profile.join("file/content")).unwrap();
    for path in ["local.txt", "content/local.txt", "contentish"] {
        std::fs::write(server.profile.join(&format!("file/{path}")), "static").unwrap();
    }
    server
        .access
        .set_policy(
            access_control::Method::Unspecified,
            "/",
            access_control::Effect::Allow,
            access_control::Grantee::Anony,
        )
        .await
        .unwrap();
    let local = server.endpoint.local_authority().unwrap();
    for (location, path, expected_status, expected) in [
        ("/", "/std/file/local.txt", http::StatusCode::OK, "static"),
        (
            "/",
            "/file/local.txt",
            http::StatusCode::OK,
            "upstream /file/local.txt",
        ),
        ("= /file", "/file", http::StatusCode::OK, "upstream /file"),
        (
            "= /file/content",
            "/file/content?path=README.md",
            http::StatusCode::OK,
            "upstream /file/content?path=README.md",
        ),
        (
            "= /file/content",
            "/file/content/local.txt",
            http::StatusCode::NOT_FOUND,
            "Not Found",
        ),
        (
            "/file/content",
            "/file/content/local.txt",
            http::StatusCode::OK,
            "upstream /file/content/local.txt",
        ),
        (
            "/file/content",
            "/file/contentish",
            http::StatusCode::NOT_FOUND,
            "Not Found",
        ),
        (
            "/file",
            "/file/local.txt",
            http::StatusCode::OK,
            "upstream /file/local.txt",
        ),
        (
            "/",
            "/std/file/missing",
            http::StatusCode::NOT_FOUND,
            "Not Found",
        ),
        (
            "/",
            "/api/echo/not-declared",
            http::StatusCode::NOT_FOUND,
            "Not Found",
        ),
        (
            "/",
            "/api/echo/upload",
            http::StatusCode::METHOD_NOT_ALLOWED,
            "Method Not Allowed",
        ),
        (
            "/",
            "/api/echo-other/upload",
            http::StatusCode::OK,
            "upstream /api/echo-other/upload",
        ),
        (
            "/",
            "/app/unloaded/",
            http::StatusCode::OK,
            "upstream /app/unloaded/",
        ),
        (
            "/",
            "/std/api/echo/upload",
            http::StatusCode::NOT_FOUND,
            "Not Found",
        ),
        ("/", "/std", http::StatusCode::NOT_FOUND, "Not Found"),
        (
            "/",
            "/std/missing",
            http::StatusCode::NOT_FOUND,
            "Not Found",
        ),
        (
            "/",
            "/api/unloaded/run",
            http::StatusCode::OK,
            "upstream /api/unloaded/run",
        ),
        (
            "/",
            "/std-extra",
            http::StatusCode::OK,
            "upstream /std-extra",
        ),
        (
            "/",
            "/api/note/upload",
            http::StatusCode::OK,
            "upstream /api/note/upload",
        ),
        ("/", "/contact", http::StatusCode::OK, "upstream /contact"),
        ("/", "/contacts", http::StatusCode::OK, "upstream /contacts"),
        (
            "/",
            "/acl/access",
            http::StatusCode::OK,
            "upstream /acl/access",
        ),
        (
            "/",
            "/workspace/",
            http::StatusCode::OK,
            "upstream /workspace/",
        ),
        (
            "/",
            "/workspace-api/context",
            http::StatusCode::OK,
            "upstream /workspace-api/context",
        ),
        (
            "/",
            "/chat-api/context",
            http::StatusCode::OK,
            "upstream /chat-api/context",
        ),
        (
            "/",
            "/pishoo/settings",
            http::StatusCode::OK,
            "upstream /pishoo/settings",
        ),
        (
            "/",
            "/.pishoo/dhttp/target",
            http::StatusCode::OK,
            "upstream /.pishoo/dhttp/target",
        ),
    ] {
        let mut parts = format!("http://{address}")
            .parse::<http::Uri>()
            .unwrap()
            .into_parts();
        parts.path_and_query = None;
        server.config.proxy_locations = vec![crate::setup::ProxyLocation {
            location: location.into(),
            proxy_pass: parts,
        }];
        let mut request = Request::builder()
            .uri(path)
            .body(AxumBody::empty())
            .unwrap();
        request.extensions_mut().insert(dhttp::HandshakeSummary {
            alpn: None,
            local: Some(local.clone()),
            remote: None,
        });
        let response = current_router(&server).oneshot(request).await.unwrap();
        assert_eq!(response.status(), expected_status, "{location}: {path}");
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            expected,
            "{location}: {path}"
        );
    }
    upstream.abort();
}

#[tokio::test(start_paused = true)]
async fn ocsp_refresh_starts_after_three_days_and_skips_missed_ticks() {
    use std::time::Duration;
    let mut ticks = ocsp_ticks();
    let started = Instant::now();
    let period = Duration::from_secs(72 * 60 * 60);
    assert!(
        tokio::time::timeout(period - Duration::from_secs(1), ticks.tick())
            .await
            .is_err()
    );
    assert_eq!(ticks.tick().await, started + period);
    tokio::time::advance(period * 4 + Duration::from_secs(1)).await;
    ticks.tick().await;
    // A delayed runtime performs one renewal rather than replaying every missed day.
    assert!(
        tokio::time::timeout(Duration::from_secs(1), ticks.tick())
            .await
            .is_err()
    );
    assert_eq!(ticks.tick().await, started + period * 6);
}

#[tokio::test]
async fn failed_ocsp_refresh_preserves_loaded_credentials_and_cache() {
    let root = tempfile::tempdir().unwrap();
    let mut server = server(root.path()).await;
    let before = server.endpoint.local_authority().unwrap();
    std::fs::write(server.profile.ocsp_path(), b"existing cache").unwrap();
    // This fixture has no responder URI; fetching must fail before changing any resource.
    assert!(renew_ocsp(&server.profile, &server.endpoint).await.is_err());
    assert_eq!(
        server.endpoint.local_authority().unwrap().ocsp(),
        before.ocsp()
    );
    assert_eq!(
        std::fs::read(server.profile.ocsp_path()).unwrap(),
        b"existing cache"
    );
    assert!(!server.sandbox.read().unwrap().libs.is_empty());
    server.close().await.unwrap();
}

fn component(version: &str) -> Vec<u8> {
    let mut bytes = include_bytes!("../fixtures/wasi-http-read-request-then-respond.wasm").to_vec();
    fn leb(mut n: usize, out: &mut Vec<u8>) {
        loop {
            let byte = (n & 127) as u8;
            n >>= 7;
            out.push(byte | if n > 0 { 128 } else { 0 });
            if n == 0 {
                break;
            }
        }
    }
    let document = format!(
        r#"{{"openapi":"3.1.0","info":{{"title":"test","version":"{version}"}},"paths":{{"/upload":{{"post":{{"responses":{{"200":{{"description":"ok"}}}}}}}}}}}}"#
    );
    let mut section = Vec::new();
    leb(14, &mut section);
    section.extend_from_slice(b"pishoo:openapi");
    section.extend(document.as_bytes());
    bytes.push(0);
    leb(section.len(), &mut bytes);
    bytes.extend(section);
    bytes
}
async fn server(root: &std::path::Path) -> Server {
    let profile = IdentityProfile::try_from(root.join("alice")).unwrap();
    std::fs::create_dir_all(profile.db_dir()).unwrap();
    std::fs::create_dir_all(profile.join("ssl")).unwrap();
    std::fs::create_dir_all(profile.join("lib/echo")).unwrap();
    let db = rusqlite::Connection::open(profile.db_dir().join("config.db")).unwrap();
    db.execute_batch("PRAGMA user_version=1; CREATE TABLE settings(listen INTEGER, enabled INTEGER NOT NULL DEFAULT 1); INSERT INTO settings(listen) VALUES(0); CREATE TABLE proxy_locations(location TEXT,proxy_pass TEXT);").unwrap();
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    crate::setup::load_server_config(&profile).unwrap();
    crate::sandbox::install_lib(
        &profile,
        "echo",
        &crate::sandbox::tests::package_component(&component("1")),
        &runtime,
    )
    .unwrap();
    let endpoint = crate::test_identity::endpoint(profile.name());
    let certificates = endpoint.local_authority().unwrap();
    let pem = certificates
        .certificates()
        .iter()
        .map(|cert| {
            use base64::Engine as _;
            format!(
                "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
                base64::engine::general_purpose::STANDARD.encode(cert.as_ref())
            )
        })
        .collect::<String>();
    std::fs::write(profile.cert_path(), pem).unwrap();
    let config = load_server_config(&profile).unwrap();
    let access = Arc::new(
        access_control::AccessService::load_from_db(
            "sqlite::memory:",
            endpoint.name(),
            &access_control::SubjectId::new(b"owner").unwrap(),
        )
        .await
        .unwrap(),
    );
    let mut sandbox = Sandbox::new(runtime);
    sandbox.load_libs(&profile).unwrap();
    let server = Server {
        profile,
        endpoint,
        config,
        access,
        router: Arc::new(RwLock::new(Router::new())),
        sandbox: Arc::new(RwLock::new(sandbox)),
        publisher: None,
        listener: None,
    };
    *server.router.write().unwrap() = current_router(&server);
    server
}

#[tokio::test]
async fn dhttp_route_matches_target_root_with_or_without_slash_and_child_paths() {
    let root = tempfile::tempdir().unwrap();
    let server = server(root.path()).await;

    let name = server.endpoint.name();
    let cert = rcgen::generate_simple_self_signed(vec![name.into()]).unwrap();
    let local = dhttp::LocalAuthority::new(
        &qtls::default_provider(),
        Arc::from(name),
        vec![cert.cert.der().clone()],
        qtls::PrivateKeyDer::try_from(cert.signing_key.serialize_der()).unwrap(),
        vec![1],
    )
    .unwrap();

    for path in [
        "/std/dhttp/bob.dhttp.net",
        "/std/dhttp/bob.dhttp.net/",
        "/std/dhttp/bob.dhttp.net/a/b",
    ] {
        server
            .access
            .set_policy(
                access_control::Method::Specified(http::Method::GET),
                path,
                access_control::Effect::Allow,
                access_control::Grantee::Anony,
            )
            .await
            .unwrap();
        let mut request = Request::builder()
            .uri(path)
            .body(AxumBody::empty())
            .unwrap();
        request.extensions_mut().insert(dhttp::HandshakeSummary {
            alpn: None,
            local: Some(local.clone()),
            remote: None,
        });
        let router = server.router.read().unwrap().clone();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), http::StatusCode::FORBIDDEN, "{path}");
    }
}

#[tokio::test]
async fn lib_routes_use_existing_access_rules() {
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement, TryGetable};

    let root = tempfile::tempdir().unwrap();
    let server = server(root.path()).await;

    let rule = server
        .access
        .database()
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT effect, grantee FROM access_rules WHERE method = 'POST' AND api = '/api/echo/upload'",
        ))
        .await
        .unwrap();
    assert!(rule.is_none());
    let headers = access_control::Headers {
        method: http::Method::POST,
        path: "/api/echo/upload".into(),
        fields: http::HeaderMap::new(),
    };
    assert!(matches!(
        server
            .access
            .auth(headers.clone(), None, None)
            .await
            .unwrap(),
        access_control::AuthResult::Denied
    ));
    let owner = access_control::SubjectId::new(b"owner").unwrap();
    assert!(matches!(
        server
            .access
            .auth(headers, Some(server.endpoint.name()), Some(&owner))
            .await
            .unwrap(),
        access_control::AuthResult::Allowed
    ));

    server
        .access
        .set_policy(
            access_control::Method::Specified(http::Method::POST),
            "/api/echo/upload",
            access_control::Effect::Allow,
            access_control::Grantee::Anony,
        )
        .await
        .unwrap();

    let rule = server
        .access
        .database()
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT effect, grantee FROM access_rules WHERE method = 'POST' AND api = '/api/echo/upload'",
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(String::try_get(&rule, "", "effect").unwrap(), "allow");
    assert_eq!(String::try_get(&rule, "", "grantee").unwrap(), "?");
}

#[tokio::test]
async fn close_clears_router_and_execution_resources() {
    let root = tempfile::tempdir().unwrap();
    let mut server = server(root.path()).await;
    server.close().await.unwrap();
    assert!(
        server.sandbox.read().unwrap().libs.is_empty()
            && server.sandbox.read().unwrap().tasks.is_closed()
    );
    let router = server.router.read().unwrap().clone();
    let response = router
        .oneshot(
            Request::builder()
                .uri("/api/echo/upload")
                .body(AxumBody::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), http::StatusCode::NOT_FOUND);
    server.close().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn lib_root_symlink_does_not_grant_a_foreign_directory() {
    let root = tempfile::tempdir().unwrap();
    let server = server(root.path()).await;
    let path = server.profile.join("lib");
    std::fs::rename(&path, server.profile.join("real-lib")).unwrap();
    std::os::unix::fs::symlink(server.profile.join("real-lib"), &path).unwrap();
    assert!(
        server
            .sandbox
            .write()
            .unwrap()
            .load_libs(&server.profile)
            .is_err()
    );
}

#[tokio::test]
async fn disabled_listener_is_rejected_before_network_registration() {
    let root = tempfile::tempdir().unwrap();
    let server = server(root.path()).await;
    assert!(matches!(
        server.listen().await,
        Err(Error::InvalidConfig(_))
    ));
    assert!(server.publisher.is_none());
}

#[tokio::test]
async fn disabled_profile_skips_credentials_libs_and_workers() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("disabled")).unwrap();
    std::fs::create_dir_all(profile.db_dir()).unwrap();
    std::fs::create_dir_all(profile.join("lib/broken")).unwrap();
    std::fs::write(profile.join("lib/broken/lib.webc"), b"broken").unwrap();
    let db = rusqlite::Connection::open(profile.config_db_path()).unwrap();
    db.execute_batch("PRAGMA user_version=1; CREATE TABLE settings(listen INTEGER, enabled INTEGER); INSERT INTO settings VALUES(2, 0); CREATE TABLE proxy_locations(location TEXT, proxy_pass TEXT);").unwrap();
    assert!(
        load_profile(profile.clone(), Arc::new(WasmRuntime::new().unwrap()))
            .await
            .unwrap()
            .is_none()
    );
    for name in ["access.db", "workspace.db", "chat.db"] {
        assert!(!profile.db_dir().join(name).exists());
    }
}

#[tokio::test]
async fn live_scan_updates_proxies_preserves_invalid_config_and_disables_server() {
    let root = tempfile::tempdir().unwrap();
    let server = server(root.path()).await;
    let name = server.name().to_owned();
    let profile = server.profile.clone();
    let sandbox_lib = server
        .sandbox
        .read()
        .unwrap()
        .libs
        .values()
        .next()
        .unwrap()
        .clone();
    let runtime = server.sandbox.read().unwrap().runtime.clone();
    let old_router = server.router.read().unwrap().clone();
    let home = DhttpHome::new(root.path().to_path_buf());
    let mut servers = BTreeMap::from([(name.clone(), server)]);
    crate::setup::config_database(
        &profile,
        &http::Method::PATCH,
        &"/std/pishoo/proxies".parse().unwrap(),
        Some(serde_json::json!({"location":"/live", "proxy_pass":"127.0.0.1:8080"})),
    )
    .unwrap();
    assert!(
        reconcile_profiles(&home, &mut servers, runtime.clone())
            .await
            .unwrap()
    );
    assert!(
        !reconcile_profiles(&home, &mut servers, runtime.clone())
            .await
            .unwrap()
    );
    assert_eq!(servers[&name].config.proxy_locations[0].location, "/live");
    assert!(Arc::ptr_eq(
        &sandbox_lib,
        servers[&name]
            .sandbox
            .read()
            .unwrap()
            .libs
            .values()
            .next()
            .unwrap()
    ));
    let db = rusqlite::Connection::open(profile.config_db_path()).unwrap();
    db.execute("UPDATE settings SET listen=99", []).unwrap();
    reconcile_profiles(&home, &mut servers, runtime.clone())
        .await
        .unwrap();
    assert_eq!(servers[&name].config.listen, 0);
    db.execute("UPDATE settings SET listen=0", []).unwrap();
    std::fs::remove_file(profile.config_db_path()).unwrap();
    reconcile_profiles(&home, &mut servers, runtime.clone())
        .await
        .unwrap();
    assert!(servers.contains_key(&name));
    assert!(!profile.config_db_path().exists());
    drop(db);
    let db = rusqlite::Connection::open(profile.config_db_path()).unwrap();
    db.execute_batch("PRAGMA user_version=1; CREATE TABLE settings(listen INTEGER, enabled INTEGER); INSERT INTO settings VALUES(0, 0); CREATE TABLE proxy_locations(location TEXT, proxy_pass TEXT);").unwrap();
    reconcile_profiles(&home, &mut servers, runtime.clone())
        .await
        .unwrap();
    assert!(servers.is_empty());
    // Subsequent scans keep a disabled profile unloaded, even with unusable credentials.
    reconcile_profiles(&home, &mut servers, runtime)
        .await
        .unwrap();
    assert!(servers.is_empty());
    drop(old_router);
}

#[tokio::test]
async fn discovery_failure_retains_servers_and_deleted_identity_is_removed() {
    let root = tempfile::tempdir().unwrap();
    let server = server(root.path()).await;
    let runtime = server.sandbox.read().unwrap().runtime.clone();
    let mut servers = BTreeMap::from([(server.name().to_owned(), server)]);
    let missing_home = DhttpHome::new(root.path().join("missing"));
    assert!(
        reconcile_profiles(&missing_home, &mut servers, runtime.clone())
            .await
            .is_err()
    );
    assert_eq!(servers.len(), 1);
    std::fs::rename(root.path().join("alice"), root.path().join(".removed")).unwrap();
    reconcile_profiles(
        &DhttpHome::new(root.path().to_path_buf()),
        &mut servers,
        runtime,
    )
    .await
    .unwrap();
    assert!(servers.is_empty());
}

#[tokio::test]
async fn unloaded_empty_configuration_waits_for_credential_validation_before_initializing() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("empty")).unwrap();
    std::fs::create_dir_all(profile.db_dir()).unwrap();
    std::fs::write(profile.config_db_path(), []).unwrap();
    assert!(
        load_profile(profile.clone(), Arc::new(WasmRuntime::new().unwrap()))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(profile.config_db_path().metadata().unwrap().len(), 0);
    assert!(!profile.access_db_path().exists());
}

#[test]
#[ignore = "run alone: selects process-wide default artifact directory"]
fn default_artifacts_install_as_user_resources_and_are_not_reinstalled_after_removal() {
    use crate::sandbox::tests::{package_fixture, publisher};
    use serde_json::json;
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("defaults");
    std::fs::create_dir(&source).unwrap();
    let previous = std::env::var_os("PISHOO_DEFAULT_ARTIFACTS");
    unsafe {
        std::env::set_var("PISHOO_DEFAULT_ARTIFACTS", &source);
    }
    let _environment = scopeguard::guard(previous, |value| unsafe {
        match value {
            Some(value) => std::env::set_var("PISHOO_DEFAULT_ARTIFACTS", value),
            None => std::env::remove_var("PISHOO_DEFAULT_ARTIFACTS"),
        }
    });
    let authority = publisher("alice.dhttp.net", 'a');
    let mut backend = json!({"openapi":"3.1.0","info":{"title":"Defaults","version":"1.0.0","description":"Ordinary defaults","license":{"name":"MIT"}},"x-pishoo":{"kind":"lib","publisher":"alice.dhttp.net","name":"default","host":{}},"paths":{"/run":{"post":{"operationId":"run","summary":"Run"}}}});
    let mut chat = backend.clone();
    chat["x-pishoo"]["name"] = json!("chat");
    let chat_bytes = package_fixture(
        &chat,
        include_bytes!("../fixtures/wasi-http-read-request-then-respond.wasm"),
        webc::v3::write::Directory::default(),
        &authority,
    );
    std::fs::write(source.join("zzz-chat.webc"), &chat_bytes).unwrap();
    backend["x-pishoo"]["dependencies"] =
        json!([{"publisher":"alice.dhttp.net","name":"chat","version":"1.0.0"}]);
    let bytes = package_fixture(
        &backend,
        include_bytes!("../fixtures/wasi-http-read-request-then-respond.wasm"),
        webc::v3::write::Directory::default(),
        &authority,
    );
    std::fs::write(source.join("backend.webc"), &bytes).unwrap();
    let frontend = json!({"openapi":"3.1.0","info":{"title":"Defaults","version":"1.0.0","description":"Ordinary defaults","license":{"name":"MIT"}},"x-pishoo":{"kind":"app","publisher":"alice.dhttp.net","name":"default","host":{},"entry":"/","dependencies":[{"publisher":"alice.dhttp.net","name":"default","version":"1.0.0"}]},"paths":{"/":{"get":{"operationId":"page","summary":"Page","x-pishoo":{"static-file":"/public/index.html"}},"head":{"operationId":"head","summary":"Headers","x-pishoo":{"static-file":"/public/index.html"}}}}});
    let public = webc::v3::write::Directory::new(
        std::collections::BTreeMap::from([(
            "index.html".parse().unwrap(),
            b"ordinary frontend".to_vec().into(),
        )]),
        webc::v3::Timestamps::default(),
    );
    let app = package_fixture(&frontend, &[], public, &authority);
    std::fs::write(source.join("frontend.webc"), &app).unwrap();

    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir(profile.path()).unwrap();
    initialize_directories(&profile).unwrap();
    let runtime = WasmRuntime::new().unwrap();
    install_default_artifacts(&profile, &runtime).unwrap();
    assert!(profile.config_db_path().is_file());
    assert_eq!(
        std::fs::read(profile.join("lib/alice.dhttp.net/default/lib.webc")).unwrap(),
        bytes
    );
    assert_eq!(
        std::fs::read(profile.join("app/alice.dhttp.net/default/app.webc")).unwrap(),
        app
    );
    crate::sandbox::remove_app(&profile, "alice.dhttp.net/default").unwrap();
    crate::sandbox::remove_lib(&profile, "alice.dhttp.net/default").unwrap();
    install_default_artifacts(&profile, &runtime).unwrap();
    assert!(!profile.join("lib/alice.dhttp.net/default").exists());
    assert!(!profile.join("app/alice.dhttp.net/default").exists());
    assert!(profile.join("lib/alice.dhttp.net/chat/lib.webc").is_file());
    let failed = IdentityProfile::try_from(root.path().join("failed")).unwrap();
    std::fs::create_dir(failed.path()).unwrap();
    initialize_directories(&failed).unwrap();
    std::fs::write(source.join("frontend.webc"), b"invalid package").unwrap();
    assert!(install_default_artifacts(&failed, &runtime).is_err());
    assert!(!failed.config_db_path().exists());
    assert!(
        std::fs::read_dir(failed.join("lib"))
            .unwrap()
            .next()
            .is_none()
    );
}
