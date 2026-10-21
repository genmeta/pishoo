use std::collections::BTreeMap;

use dhttp_home::DhttpHome;
use tokio::time::Instant;

use super::*;
use super::{
    ocsp::renew_ocsp,
    run::{discover_profiles, load_profile, ocsp_ticks, parse_account_homes, reconcile_profiles},
};
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
    std::fs::create_dir_all(server.profile.join(".volume/sys/file/.file/content")).unwrap();
    for path in ["local.txt", "content/local.txt", "contentish"] {
        std::fs::write(
            server
                .profile
                .join(&format!(".volume/sys/file/.file/{path}")),
            "static",
        )
        .unwrap();
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
            "/echo/upload",
            http::StatusCode::METHOD_NOT_ALLOWED,
            "Method Not Allowed",
        ),
        (
            "/",
            "/echo/missing",
            http::StatusCode::NOT_FOUND,
            "Not Found",
        ),
        (
            "/",
            "/echoish/upload",
            http::StatusCode::OK,
            "upstream /echoish/upload",
        ),
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
            http::StatusCode::OK,
            "upstream /api/echo/not-declared",
        ),
        (
            "/",
            "/api/echo/upload",
            http::StatusCode::OK,
            "upstream /api/echo/upload",
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
    std::fs::create_dir_all(profile.join(".volume/sys/setting")).unwrap();
    std::fs::create_dir_all(profile.join("ssl")).unwrap();
    std::fs::create_dir_all(profile.join("log")).unwrap();
    std::fs::create_dir_all(profile.join("lib/echo")).unwrap();
    initialize_directories(&profile).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let db =
        crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    db.execute("INSERT INTO setting VALUES('listen','loopback')", [])
        .unwrap();
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    crate::setup::load_server_config(&profile).unwrap();
    crate::package::install_lib(
        &profile,
        "echo",
        &crate::package::tests::package_component(&component("1")),
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
        config_db: Mutex::new(
            crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap(),
        ),
        contact_db: Arc::new(Mutex::new(crate::contact::database(&profile).unwrap())),
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
    let mut server = server(root.path()).await;
    server.config.listen = "off".into();
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
    std::fs::create_dir_all(profile.join(".volume/sys/setting")).unwrap();
    std::fs::create_dir_all(profile.join("lib/broken")).unwrap();
    std::fs::write(profile.join("lib/broken/.webc-proof"), b"broken").unwrap();
    let db = rusqlite::Connection::open(profile.join(".volume/sys/setting/.db")).unwrap();
    db.execute_batch("PRAGMA user_version=1; CREATE TABLE settings(listen INTEGER, enabled INTEGER); INSERT INTO settings VALUES(2, 0); CREATE TABLE proxy_locations(location TEXT, proxy_pass TEXT);").unwrap();
    assert!(
        load_profile(profile.clone(), Arc::new(WasmRuntime::new().unwrap()))
            .await
            .unwrap()
            .is_none()
    );
    for name in ["access.db", "workspace.db", "chat.db"] {
        assert!(!profile.join(".volume/sys/setting").join(name).exists());
    }
}

#[tokio::test]
async fn live_scan_updates_proxies_preserves_invalid_config_and_disables_server() {
    let root = tempfile::tempdir().unwrap();
    dhttp::DhttpNetwork::init().await.unwrap();
    let mut server = server(root.path()).await;
    server.listener = Some(server.listen().await.unwrap());
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
        &"/std/sys/listen".parse().unwrap(),
        Some(serde_json::json!({"location":"/live", "proxy_pass":"127.0.0.1:8080"})),
    )
    .unwrap();
    assert!(
        reconcile_profiles(
            &ddns::mdns::MdnsResolverSet::new("pishoo-test.local"),
            &home,
            None,
            &mut servers,
            runtime.clone()
        )
        .await
        .unwrap()
    );
    assert!(
        !reconcile_profiles(
            &ddns::mdns::MdnsResolverSet::new("pishoo-test.local"),
            &home,
            None,
            &mut servers,
            runtime.clone()
        )
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
    let db =
        crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    db.execute("UPDATE setting SET value='invalid' WHERE key='listen'", [])
        .unwrap();
    reconcile_profiles(
        &ddns::mdns::MdnsResolverSet::new("pishoo-test.local"),
        &home,
        None,
        &mut servers,
        runtime.clone(),
    )
    .await
    .unwrap();
    assert_eq!(servers[&name].config.listen, "loopback");
    // Invalid SQL contents keep the loaded configuration; a repaired row is
    // visible through the same long-lived connection on the next scan.
    db.execute("UPDATE setting SET value='off' WHERE key='listen'", [])
        .unwrap();
    assert!(
        !reconcile_profiles(
            &ddns::mdns::MdnsResolverSet::new("pishoo-test.local"),
            &home,
            None,
            &mut servers,
            runtime.clone(),
        )
        .await
        .unwrap()
    );
    assert!(servers.contains_key(&name));
    assert_ne!(servers[&name].config.listen, "off");
    assert!(servers[&name].config_db.lock().unwrap().is_autocommit());
    db.execute("DELETE FROM setting WHERE key='listen'", [])
        .unwrap();
    reconcile_profiles(
        &ddns::mdns::MdnsResolverSet::new("pishoo-test.local"),
        &home,
        None,
        &mut servers,
        runtime.clone(),
    )
    .await
    .unwrap();
    assert!(servers.is_empty());
    // Subsequent scans keep a disabled profile unloaded, even with unusable credentials.
    reconcile_profiles(
        &ddns::mdns::MdnsResolverSet::new("pishoo-test.local"),
        &home,
        None,
        &mut servers,
        runtime,
    )
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
        reconcile_profiles(
            &ddns::mdns::MdnsResolverSet::new("pishoo-test.local"),
            &missing_home,
            None,
            &mut servers,
            runtime.clone()
        )
        .await
        .is_err()
    );
    assert_eq!(servers.len(), 1);
    std::fs::rename(root.path().join("alice"), root.path().join(".removed")).unwrap();
    reconcile_profiles(
        &ddns::mdns::MdnsResolverSet::new("pishoo-test.local"),
        &DhttpHome::new(root.path().to_path_buf()),
        None,
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
    std::fs::create_dir_all(profile.join(".volume/sys/setting")).unwrap();
    std::fs::write(profile.join(".volume/sys/setting/.db"), []).unwrap();
    assert!(
        load_profile(profile.clone(), Arc::new(WasmRuntime::new().unwrap()))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        profile
            .join(".volume/sys/setting/.db")
            .metadata()
            .unwrap()
            .len(),
        0
    );
    assert!(!profile.join(".volume/sys/access/.db").exists());
}

#[tokio::test]
async fn native_capability_catalog_is_installed_business_metadata_and_uses_daccess() {
    use http::{Method, StatusCode};
    use serde_json::json;
    let root = tempfile::tempdir().unwrap();
    let mut server = server(root.path()).await;
    server.access = load_access(
        &server.profile,
        &access_control::SubjectId::new(b"owner").unwrap(),
    )
    .await
    .unwrap();
    *server.router.write().unwrap() = current_router(&server);
    let doc = json!({"openapi":"3.1.0","info":{"title":"Message","version":"1.0.0","description":"test","license":{"name":"MIT"}},
        "x-pishoo":{"kind":"lib","publisher":"alice.dhttp.net","name":"message","dependencies":[],"host":{"request-identity":"identity"}},
        "x-access-groups":{"message":{"title":"1v1聊天","operations":["receive"]}},
        "paths":{"/message":{"post":{"operationId":"receive","summary":"receive","x-access-group":"message","responses":{"200":{"description":"ok"}}}}}});
    let package = crate::package::tests::package_fixture(
        &doc,
        include_bytes!("../fixtures/wasi-http-read-request-then-respond.wasm"),
        webc::v3::write::Directory::default(),
        &crate::package::tests::publisher("alice.dhttp.net", 'a'),
    );
    let runtime = server.sandbox.read().unwrap().runtime.clone();
    crate::package::install_lib(
        &server.profile,
        "alice.dhttp.net/message",
        &package,
        &runtime,
    )
    .unwrap();
    let request = || {
        let mut request = Request::builder()
            .uri("/std/acl/permissions")
            .body(AxumBody::empty())
            .unwrap();
        request.extensions_mut().insert(dhttp::HandshakeSummary {
            alpn: None,
            local: Some(server.endpoint.local_authority().unwrap()),
            remote: None,
        });
        request
    };
    assert_eq!(
        server
            .router
            .read()
            .unwrap()
            .clone()
            .oneshot(request())
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    server
        .access
        .set_policy(
            access_control::Method::Specified(Method::GET),
            "/std/acl/permissions",
            access_control::Effect::Deny,
            access_control::Grantee::All,
        )
        .await
        .unwrap();
    assert_eq!(
        server
            .router
            .read()
            .unwrap()
            .clone()
            .oneshot(request())
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    server
        .access
        .set_policy(
            access_control::Method::Specified(Method::GET),
            "/std/acl/permissions",
            access_control::Effect::Allow,
            access_control::Grantee::All,
        )
        .await
        .unwrap();
    let response = server
        .router
        .read()
        .unwrap()
        .clone()
        .oneshot(request())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[http::header::CACHE_CONTROL], "no-store");
    assert_eq!(
        response.headers()[http::header::CONTENT_TYPE],
        "application/json"
    );
    let bytes = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    let values: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        values,
        json!({"alice.dhttp.net/message · 1v1聊天":{"/message/message":["POST"]}})
    );
    // Builtin ACL, Contact and Package do not become WASM capabilities.
    assert_eq!(values.as_object().unwrap().len(), 1);
    crate::package::remove_lib(&server.profile, "alice.dhttp.net/message").unwrap();
    let response = server
        .router
        .read()
        .unwrap()
        .clone()
        .oneshot(request())
        .await
        .unwrap();
    assert_eq!(
        axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap(),
        "{}"
    );
    let db =
        crate::setup::open_config(&server.profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
            .unwrap();
    db.execute_batch("DROP TABLE lib_capability_operation; DROP TABLE lib_capability;")
        .unwrap();
    let response = server
        .router
        .read()
        .unwrap()
        .clone()
        .oneshot(request())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let bytes = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("reinstall"));
}

#[test]
fn discovery_combines_system_and_user_homes_and_rescans_new_users() {
    let root = tempfile::tempdir().unwrap();
    let home = DhttpHome::new(root.path().join("etc/.dhttp"));
    let users = root.path().join("home");
    assert!(
        discover_profiles(
            &home,
            Some(&[users.join("alice"), users.join("bob"), users.join("late")])
        )
        .unwrap()
        .is_empty()
    );
    for path in [
        home.join("system/ssl"),
        users.join("alice/.dhttp/alice/ssl"),
        users.join("bob/.dhttp/bob/ssl"),
        users.join("empty"),
    ] {
        std::fs::create_dir_all(path).unwrap();
    }
    let profiles = discover_profiles(
        &home,
        Some(&[users.join("alice"), users.join("bob"), users.join("late")]),
    )
    .unwrap();
    assert_eq!(
        profiles.iter().map(|p| p.name()).collect::<Vec<_>>(),
        ["alice.dhttp.net", "bob.dhttp.net", "system.dhttp.net"]
    );
    assert_eq!(profiles[0].path(), users.join("alice/.dhttp/alice"));
    assert_eq!(profiles[2].path(), home.join("system"));
    std::fs::create_dir_all(users.join("late/.dhttp/late/ssl")).unwrap();
    assert_eq!(
        discover_profiles(
            &home,
            Some(&[users.join("alice"), users.join("bob"), users.join("late")])
        )
        .unwrap()
        .len(),
        4
    );
    std::fs::remove_dir_all(users.join("bob")).unwrap();
    assert_eq!(
        discover_profiles(
            &home,
            Some(&[users.join("alice"), users.join("bob"), users.join("late")])
        )
        .unwrap()
        .len(),
        3
    );
    // Ordinary user runs retain single-home behavior and discovery errors.
    assert_eq!(discover_profiles(&home, None).unwrap().len(), 1);
    assert!(discover_profiles(&DhttpHome::new(root.path().join("missing")), None).is_err());
}

#[test]
fn duplicate_identity_across_homes_reports_both_paths() {
    let root = tempfile::tempdir().unwrap();
    let home = DhttpHome::new(root.path().join("etc/.dhttp"));
    let users = root.path().join("home");
    let system = home.join("alice");
    let user = users.join("alice/.dhttp/alice");
    std::fs::create_dir_all(system.join("ssl")).unwrap();
    std::fs::create_dir_all(user.join("ssl")).unwrap();
    let error = discover_profiles(&home, Some(&[users.join("alice")]))
        .unwrap_err()
        .to_string();
    assert!(error.contains("alice.dhttp.net"), "{error}");
    assert!(error.contains(system.to_str().unwrap()), "{error}");
    assert!(error.contains(user.to_str().unwrap()), "{error}");
}

#[cfg(unix)]
#[test]
fn multiuser_discovery_skips_symlinked_homes_and_profiles() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let home = DhttpHome::new(root.path().join("etc/.dhttp"));
    let users = root.path().join("home");
    let outside = root.path().join("outside");
    std::fs::create_dir_all(outside.join(".dhttp/alice/ssl")).unwrap();
    std::fs::create_dir_all(users.join("bob/.dhttp")).unwrap();
    std::fs::create_dir_all(users.join("carol")).unwrap();
    symlink(&outside, users.join("alice")).unwrap();
    symlink(outside.join(".dhttp"), users.join("carol/.dhttp")).unwrap();
    symlink(outside.join(".dhttp/alice"), users.join("bob/.dhttp/alice")).unwrap();
    assert!(
        discover_profiles(
            &home,
            Some(&[users.join("alice"), users.join("bob"), users.join("carol")])
        )
        .unwrap()
        .is_empty()
    );
}

#[tokio::test]
async fn multiuser_discovery_conflict_preserves_running_server() {
    let root = tempfile::tempdir().unwrap();
    let server = server(root.path()).await;
    let runtime = server.sandbox.read().unwrap().runtime.clone();
    let mut servers = BTreeMap::from([(server.name().to_owned(), server)]);
    let users = root.path().join("home");
    std::fs::create_dir_all(users.join("other/.dhttp/alice/ssl")).unwrap();
    assert!(
        reconcile_profiles(
            &ddns::mdns::MdnsResolverSet::new("pishoo-test.local"),
            &DhttpHome::new(root.path().to_path_buf()),
            Some(&[users.join("other")]),
            &mut servers,
            runtime
        )
        .await
        .is_err()
    );
    assert_eq!(servers.len(), 1);
    assert_eq!(
        servers["alice.dhttp.net"].profile.path(),
        root.path().join("alice")
    );
    servers
        .get_mut("alice.dhttp.net")
        .unwrap()
        .close()
        .await
        .unwrap();
}

#[test]
fn account_database_uses_actual_homes_across_distributions() {
    let homes = parse_account_homes(
        "root:x:0:0:root:/root:/bin/sh\nalice:x:1000:1000::/home/alice:/bin/sh\nbob:x:1001:1001::/var/home/bob:/bin/sh\ncustom:x:1002:1002::/srv/users/custom:/bin/sh\nshared:x:1003:1003::/srv/users/custom:/bin/sh\ninvalid:x:1004:1004::relative:/bin/sh\n",
    );
    assert_eq!(
        homes,
        ["/home/alice", "/root", "/srv/users/custom", "/var/home/bob"]
            .map(std::path::PathBuf::from)
    );
}

#[tokio::test]
async fn workspace_static_defaults_allow_anonymous_reads_without_management_access() {
    use std::collections::BTreeMap;

    use http::{Method, StatusCode};
    use serde_json::json;
    use webc::v3::{
        Timestamps,
        write::{DirEntry, Directory},
    };
    let root = tempfile::tempdir().unwrap();
    let mut server = server(root.path()).await;
    server.access = load_access(
        &server.profile,
        &access_control::SubjectId::new(b"owner").unwrap(),
    )
    .await
    .unwrap();
    let document = json!({
        "info":{"title":"Workspace","version":"1.0.0","description":"Public introduction","license":{"name":"MIT"}},
        "x-pishoo":{"kind":"app","publisher":"std.dhttp.net","name":"workspace","entry":"/","dependencies":[]}
    });
    let public = Directory::new(
        BTreeMap::from([
            (
                "index.html".parse().unwrap(),
                b"public introduction".to_vec().into(),
            ),
            (
                "assets".parse().unwrap(),
                DirEntry::Dir(Directory::new(
                    BTreeMap::from([("app.js".parse().unwrap(), b"export {}".to_vec().into())]),
                    Timestamps::default(),
                )),
            ),
        ]),
        Timestamps::default(),
    );
    let package = crate::package::tests::package_fixture(
        &document,
        &[],
        public,
        &crate::package::tests::publisher("std.dhttp.net", 'a'),
    );
    let mut other_document = document.clone();
    other_document["x-pishoo"]["publisher"] = json!("other.dhttp.net");
    let other_package = crate::package::tests::package_fixture(
        &other_document,
        &[],
        Directory::new(
            BTreeMap::from([("index.html".parse().unwrap(), b"other".to_vec().into())]),
            Timestamps::default(),
        ),
        &crate::package::tests::publisher("other.dhttp.net", 'b'),
    );
    assert!(
        crate::package::install_app(&server.profile, "other.dhttp.net/workspace", &other_package)
            .unwrap_err()
            .to_string()
            .contains("existing ACL")
    );
    server
        .access
        .set_policy(
            access_control::Method::Specified(Method::GET),
            "/workspace",
            access_control::Effect::Deny,
            access_control::Grantee::Named,
        )
        .await
        .unwrap();
    assert!(
        crate::package::install_app(&server.profile, "std.dhttp.net/workspace", &package)
            .unwrap_err()
            .to_string()
            .contains("existing ACL")
    );
    // daccess replaces conflicting All/Named rules rather than layering them.
    // Restore the public default after testing an explicit administrator override.
    server
        .access
        .set_policy(
            access_control::Method::Specified(Method::GET),
            "/workspace",
            access_control::Effect::Allow,
            access_control::Grantee::All,
        )
        .await
        .unwrap();
    crate::package::install_app(&server.profile, "std.dhttp.net/workspace", &package).unwrap();
    let profile_document = json!({
        "openapi":"3.1.0",
        "info":{"title":"Profile","version":"1.0.0","description":"Public profile","license":{"name":"MIT"}},
        "x-pishoo":{"kind":"lib","publisher":"std.dhttp.net","name":"profile","dependencies":[],"host":{}},
        "paths":{
            "/profile/nickname":{"get":{"operationId":"nickname","summary":"Nickname"},"put":{"operationId":"edit","summary":"Edit nickname"}},
            "/profile/avatar":{"get":{"operationId":"avatar","summary":"Avatar"}}
        }
    });
    let profile_package = crate::package::tests::package_fixture(
        &profile_document,
        include_bytes!("../fixtures/wasi-http-read-request-then-respond.wasm"),
        Directory::default(),
        &crate::package::tests::publisher("std.dhttp.net", 'a'),
    );
    let runtime = server.sandbox.read().unwrap().runtime.clone();
    crate::package::install_lib(
        &server.profile,
        "std.dhttp.net/profile",
        &profile_package,
        &runtime,
    )
    .unwrap();
    server.sandbox.write().unwrap().apps =
        crate::package::installed_apps(&server.profile, None).unwrap();
    *server.router.write().unwrap() = current_router(&server);
    let request = |method: Method, path: &str| {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .body(AxumBody::empty())
            .unwrap();
        request.extensions_mut().insert(dhttp::HandshakeSummary {
            alpn: None,
            local: Some(server.endpoint.local_authority().unwrap()),
            remote: None,
        });
        request
    };
    for method in [Method::GET, Method::HEAD] {
        for (path, expected) in [
            ("/workspace/", b"public introduction".as_slice()),
            ("/workspace/assets/app.js", b"export {}".as_slice()),
        ] {
            let response = server
                .router
                .read()
                .unwrap()
                .clone()
                .oneshot(request(method.clone(), path))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{method} {path}");
            let body = axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap();
            assert_eq!(
                body.as_ref(),
                if method == Method::HEAD {
                    b""
                } else {
                    expected
                }
            );
        }
    }
    for (method, path) in [
        (Method::POST, "/workspace/"),
        (Method::GET, "/workspace-other/"),
        (Method::GET, "/std/contacts"),
        (Method::GET, "/std/acl/access"),
        (Method::GET, "/std/sys/listen"),
    ] {
        let response = server
            .router
            .read()
            .unwrap()
            .clone()
            .oneshot(request(method, path))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
#[ignore = "run alone; requires local multicast on loopback and an IPv4 network interface"]
async fn stopping_identity_removes_all_mdns_records_before_waiting_for_tasks() {
    use ddns::mdns::{MdnsBinding, MdnsResolverSet};
    use dhttp::resolve::Resolve;
    use futures::StreamExt;

    dhttp::DhttpNetwork::init().await.unwrap();
    let mdns = MdnsResolverSet::new(format!("pishoo-retire-{}.local", std::process::id()));
    let device = if cfg!(target_os = "linux") {
        "lo"
    } else {
        "lo0"
    };
    mdns.upsert(MdnsBinding::new(device, "127.0.0.1".parse().unwrap()))
        .await
        .unwrap();
    for (bound, device) in qprotocol::AddressBook::global().inner_bindings() {
        if bound.is_ipv4() && !bound.ip().is_loopback() {
            mdns.upsert(MdnsBinding::new(device.name(), bound.ip()))
                .await
                .unwrap();
        }
    }
    let instances = mdns.snapshot();
    assert!(
        instances.len() >= 2,
        "test requires at least two mDNS instances"
    );
    let kept = crate::test_identity::endpoint("kept.dhttp.net");
    let address: dhttp::resolve::EndpointAddr = "192.168.80.7:4433"
        .parse::<std::net::SocketAddr>()
        .unwrap()
        .into();

    for operation in ["disable", "remove", "off", "external"] {
        let root = tempfile::tempdir().unwrap();
        let mut server = server(root.path()).await;
        server.config.listen = "internal".into();
        let runtime = server.sandbox.read().unwrap().runtime.clone();
        let name = server.name().to_owned();
        for instance in &instances {
            instance
                .publish_endpoints(
                    &server.endpoint.local_authority().unwrap(),
                    &name,
                    [address],
                )
                .unwrap();
            instance
                .publish_endpoints(&kept.local_authority().unwrap(), kept.name(), [address])
                .unwrap();
            assert!(
                instance
                    .lookup(&name, "", None)
                    .await
                    .unwrap()
                    .next()
                    .await
                    .is_some()
            );
        }
        // A blocked guest must not keep stale addresses advertised while close waits.
        let (release, pending) = tokio::sync::oneshot::channel::<()>();
        if matches!(operation, "disable" | "remove") {
            server.sandbox.read().unwrap().tasks.spawn(async {
                let _ = pending.await;
            });
        }
        match operation {
            "remove" => {
                std::fs::rename(server.profile.path(), root.path().join(".removed")).unwrap()
            }
            operation => {
                let sql = match operation {
                    "disable" => "DELETE FROM setting WHERE key='listen'",
                    "off" => "DELETE FROM setting WHERE key='listen'",
                    "external" => "UPDATE settings SET listen='external'",
                    _ => unreachable!(),
                };
                rusqlite::Connection::open(server.profile.join(".volume/sys/setting/.db"))
                    .unwrap()
                    .execute_batch(sql)
                    .unwrap();
            }
        }
        let home = DhttpHome::new(root.path().to_path_buf());
        let mut servers = BTreeMap::from([(name.clone(), server)]);
        let mut reconcile = Box::pin(reconcile_profiles(
            &mdns,
            &home,
            None,
            &mut servers,
            runtime,
        ));
        if matches!(operation, "disable" | "remove") {
            tokio::select! {
                result = &mut reconcile => panic!("cleanup must wait for the guest: {result:?}"),
                _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {},
            }
        } else {
            assert!(reconcile.as_mut().await.unwrap());
        }
        for instance in &instances {
            assert!(
                instance.lookup(&name, "", None).await.is_err(),
                "{operation}: stale record on {}",
                instance.bound_device()
            );
            assert!(
                instance
                    .lookup(kept.name(), "", None)
                    .await
                    .unwrap()
                    .next()
                    .await
                    .is_some(),
                "unrelated identity must remain published"
            );
        }
        if matches!(operation, "disable" | "remove") {
            release.send(()).unwrap();
            assert!(reconcile.as_mut().await.unwrap());
        }
        drop(reconcile);
        assert_eq!(
            mdns.snapshot().len(),
            instances.len(),
            "shared mDNS query resources must remain"
        );
        for server in servers.values_mut() {
            if operation == "external" {
                assert!(server.publisher.is_some());
            }
            server.close().await.unwrap();
        }
    }
    mdns.shutdown().await.unwrap();
}

#[tokio::test]
async fn permission_catalog_qualifies_group_titles_and_preserves_legacy_registration() {
    for legacy in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
        std::fs::create_dir_all(profile.join(".volume/sys/setting")).unwrap();
        crate::setup::load_server_config(&profile).unwrap();
        let db = crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
            .unwrap();
        if legacy {
            db.execute_batch("DROP TABLE acl.lib_capability_operation; DROP TABLE acl.lib_capability; DROP TABLE lib_registry.lib_dependency; DROP TABLE lib_registry.lib;").unwrap();
            db.execute_batch("CREATE TABLE lib (id TEXT PRIMARY KEY, name TEXT, version TEXT);
                CREATE TABLE lib_capability (lib_id TEXT, capability_id TEXT, title TEXT);
                CREATE TABLE lib_capability_operation (lib_id TEXT, capability_id TEXT, path TEXT, method TEXT);
                INSERT INTO lib VALUES ('dev.dhttp.net/message', 'message', '1.0.0');
                INSERT INTO lib_capability VALUES ('dev.dhttp.net/message', 'read', 'read message'), ('dev.dhttp.net/message', 'details', 'read message'), ('dev.dhttp.net/message', 'empty', 'empty');
                INSERT INTO lib_capability_operation VALUES ('dev.dhttp.net/message', 'read', '/std/message', 'GET'), ('dev.dhttp.net/message', 'read', '/std/message', 'HEAD'), ('dev.dhttp.net/message', 'details', '/std/message', 'GET'), ('dev.dhttp.net/message', 'details', '/std/message/history', 'GET');").unwrap();
        } else {
            db.execute_batch(include_str!("../../src/package/packages.sql"))
                .unwrap();
            db.execute_batch("INSERT INTO lib (name, author, version) VALUES ('message', 'dev.dhttp.net', '1.0.0');
                INSERT INTO lib_capability VALUES ('message', 'read', 'read message'), ('message', 'details', 'read message'), ('message', 'empty', 'empty');
                INSERT INTO lib_capability_operation VALUES ('message', 'read', 'GET', '/std/message'), ('message', 'read', 'HEAD', '/std/message'), ('message', 'details', 'GET', '/std/message/history');").unwrap();
        }
        if legacy {
            db.execute_batch("INSERT INTO lib VALUES ('other.dhttp.net/archive', 'archive', '1.0.0');
                INSERT INTO lib_capability VALUES ('other.dhttp.net/archive', 'read', 'read message');
                INSERT INTO lib_capability_operation VALUES ('other.dhttp.net/archive', 'read', '/archive/entries', 'GET');").unwrap();
        } else {
            db.execute_batch("INSERT INTO lib (name, author, version) VALUES ('archive', 'other.dhttp.net', '1.0.0');
                INSERT INTO lib_capability VALUES ('archive', 'read', 'read message');
                INSERT INTO lib_capability_operation VALUES ('archive', 'read', 'GET', '/archive/entries');").unwrap();
        }
        let router = capabilities_router(profile.clone());
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/std/acl/permissions")
                    .body(AxumBody::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), http::StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let expected = serde_json::json!({"dev.dhttp.net/message · read message":{"/message/message":["GET","HEAD"],"/message/message/history":["GET"]},"dev.dhttp.net/message · empty":{},"other.dhttp.net/archive · read message":{"/archive/entries":["GET"]}});
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
            expected
        );
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/std/acl/capabilities")
                    .body(AxumBody::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), http::StatusCode::NOT_FOUND);
    }
}
