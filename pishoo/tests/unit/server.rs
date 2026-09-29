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
    db.execute_batch("PRAGMA user_version=1; CREATE TABLE settings(listen INTEGER, exec INTEGER); INSERT INTO settings VALUES(0,0); CREATE TABLE proxy_locations(location TEXT,proxy_pass TEXT);").unwrap();
    std::fs::write(profile.join("lib/echo/lib.wasm"), component("1")).unwrap();
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let endpoint = dhttp::Endpoint::load(profile.name()).await.unwrap();
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
    let proxies = config.proxy_locations.clone();
    let router = Router::new()
        .merge(access_router(
            access.clone(),
            endpoint.clone(),
            profile.name(),
            endpoint.name(),
        ))
        .merge(sandbox.api_router(endpoint.clone()))
        .merge(file_router(profile.join("file")))
        .fallback(any(move |request: Request<AxumBody>| {
            proxy_pass(proxies.clone(), request)
        }))
        .layer(axum::middleware::from_fn_with_state(
            access.clone(),
            authorize,
        ));
    let router = Arc::new(RwLock::new(router));
    Server {
        profile,
        endpoint,
        config,
        access,
        router,
        sandbox,
        exec_tasks: TaskTracker::new(),
    }
}

#[tokio::test]
async fn reload_reuses_valid_versions_and_rejects_bad_candidates() {
    let root = tempfile::tempdir().unwrap();
    let mut server = server(root.path()).await;
    let old = server.sandbox.libs["echo"].clone();
    let task = server.sandbox.tasks.token();
    server.reload().await.unwrap();
    assert_eq!(server.sandbox.tasks.len(), 1);
    assert!(Arc::ptr_eq(&old, &server.sandbox.libs["echo"]));
    std::fs::write(
        server.profile.join("lib/echo/lib.wasm"),
        b"broken component",
    )
    .unwrap();
    assert!(server.reload().await.is_err());
    assert!(Arc::ptr_eq(&old, &server.sandbox.libs["echo"]));
    std::fs::write(server.profile.join("lib/echo/lib.wasm"), component("2")).unwrap();
    server.reload().await.unwrap();
    let new = server.sandbox.libs["echo"].clone();
    assert!(!Arc::ptr_eq(&old, &new));
    assert_eq!(server.sandbox.tasks.len(), 1);
    std::fs::remove_dir_all(server.profile.join("lib/echo")).unwrap();
    server.reload().await.unwrap();
    assert!(server.sandbox.libs.is_empty());
    assert_eq!(server.sandbox.tasks.len(), 1);
    drop(task);
    assert!(server.sandbox.tasks.is_empty());
}

#[tokio::test]
async fn reload_does_not_write_access_rules_or_replace_admin_rules() {
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement, TryGetable};

    let root = tempfile::tempdir().unwrap();
    let mut server = server(root.path()).await;
    server.reload().await.unwrap();

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
        request_id: None,
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
    server.reload().await.unwrap();

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
async fn reload_failure_keeps_config_and_lib_and_close_is_permanent() {
    let root = tempfile::tempdir().unwrap();
    let mut server = server(root.path()).await;
    let old = server.sandbox.libs["echo"].clone();
    let db = rusqlite::Connection::open(server.profile.db_dir().join("config.db")).unwrap();
    db.execute("UPDATE settings SET listen=3", []).unwrap();
    assert!(matches!(
        server.reload().await,
        Err(Error::InvalidConfig(_))
    ));
    assert_eq!(server.config.listen, 0);
    assert!(Arc::ptr_eq(&old, &server.sandbox.libs["echo"]));
    db.execute("UPDATE settings SET listen=0, exec=1", [])
        .unwrap();
    assert!(matches!(
        server.reload().await,
        Err(Error::InvalidConfig(_))
    ));
    assert!(!server.config.exec);
    server.close().await.unwrap();
    assert!(server.exec_tasks.is_closed());
    assert!(server.sandbox.libs.is_empty() && server.sandbox.tasks.is_closed());
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
