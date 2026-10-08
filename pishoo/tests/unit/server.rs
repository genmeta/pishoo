use super::*;
use crate::{Error, setup::load_server_config};

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
