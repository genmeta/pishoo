use super::*;
use crate::{Error, setup::load_server_config};

#[tokio::test]
async fn explicit_file_proxies_override_only_matching_static_paths() {
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
    for (location, path, expected) in [
        ("/", "/file/local.txt", "static"),
        ("= /file", "/file", "upstream /file"),
        (
            "= /file/content",
            "/file/content?path=README.md",
            "upstream /file/content?path=README.md",
        ),
        ("= /file/content", "/file/content/local.txt", "static"),
        (
            "/file/content",
            "/file/content/local.txt",
            "upstream /file/content/local.txt",
        ),
        ("/file/content", "/file/contentish", "static"),
        ("/file", "/file/local.txt", "upstream /file/local.txt"),
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
        assert_eq!(
            response.status(),
            http::StatusCode::OK,
            "{location}: {path}"
        );
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
    assert!(!server.sandbox.libs.is_empty());
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
    db.execute_batch("PRAGMA user_version=1; CREATE TABLE settings(listen INTEGER); INSERT INTO settings VALUES(0); CREATE TABLE proxy_locations(location TEXT,proxy_pass TEXT);").unwrap();
    std::fs::write(profile.join("lib/echo/lib.wasm"), component("1")).unwrap();
    let runtime = Arc::new(WasmRuntime::new().unwrap());
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
    let subject = access_control::SubjectId::new(b"owner").unwrap();
    let workspace = Arc::new(Workspace::new(
        profile.name().to_owned(),
        endpoint.name().to_owned(),
        subject.clone(),
        WorkspaceStore::open(&profile).await.unwrap(),
        access.clone(),
    ));
    let chat = Arc::new(Chat::new(
        endpoint.name().to_owned(),
        subject,
        ChatStore::open(&profile).await.unwrap(),
        access.clone(),
    ));
    workspace.configure_chat(chat.clone()).await;
    let mut sandbox = Sandbox::new(runtime);
    sandbox.load_libs(&profile).unwrap();
    let server = Server {
        profile,
        endpoint,
        config,
        access,
        workspace,
        chat,
        router: Arc::new(RwLock::new(Router::new())),
        sandbox,
        publisher: None,
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
        "/.pishoo/dhttp/bob.dhttp.net",
        "/.pishoo/dhttp/bob.dhttp.net/",
        "/.pishoo/dhttp/bob.dhttp.net/a/b",
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
    assert!(server.sandbox.libs.is_empty() && server.sandbox.tasks.is_closed());
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
    let mut server = server(root.path()).await;
    let path = server.profile.join("lib");
    std::fs::rename(&path, server.profile.join("real-lib")).unwrap();
    std::os::unix::fs::symlink(server.profile.join("real-lib"), &path).unwrap();
    assert!(server.sandbox.load_libs(&server.profile).is_err());
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
