use super::*;

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
    let runtime = Arc::new(Runtime::new().unwrap());
    let terminal = Arc::new(TerminalManager::new(TerminalPolicy::default(), root).unwrap());
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
    let cancel = CancellationToken::new();
    let libs = load_libs(&profile, &runtime, &BTreeMap::new(), &cancel).unwrap();
    let sandbox = Arc::new(Sandbox::new());
    let router = Arc::new(RwLock::new(
        build_router(
            endpoint.clone(),
            access.clone(),
            &libs,
            &config,
            &profile,
            sandbox.clone(),
        )
        .unwrap(),
    ));
    Server {
        profile,
        endpoint,
        config,
        access,
        router,
        libs,
        runtime,
        sandbox,
        terminal,
        cancel,
    }
}

#[tokio::test]
async fn reload_reuses_valid_versions_retains_bad_candidates_and_cancels_deleted_versions() {
    let root = tempfile::tempdir().unwrap();
    let mut server = server(root.path()).await;
    let old = server.libs["echo"].clone();
    let sandbox = server.sandbox.clone();
    let permit = sandbox.lib_slots.clone().try_acquire_owned().unwrap();
    server.reload().await.unwrap();
    assert!(Arc::ptr_eq(&sandbox, &server.sandbox));
    assert_eq!(server.sandbox.lib_slots.available_permits(), 3);
    assert!(Arc::ptr_eq(&old, &server.libs["echo"]));
    std::fs::write(
        server.profile.join("lib/echo/lib.wasm"),
        b"broken component",
    )
    .unwrap();
    server.reload().await.unwrap();
    assert!(Arc::ptr_eq(&old, &server.libs["echo"]));
    assert!(!old.cancel.is_cancelled());
    std::fs::write(server.profile.join("lib/echo/lib.wasm"), component("2")).unwrap();
    server.reload().await.unwrap();
    let new = server.libs["echo"].clone();
    assert!(!Arc::ptr_eq(&old, &new));
    assert!(!old.cancel.is_cancelled());
    assert!(Arc::ptr_eq(&sandbox, &server.sandbox));
    assert_eq!(server.sandbox.lib_slots.available_permits(), 3);
    std::fs::remove_file(server.profile.join("lib/echo/lib.wasm")).unwrap();
    server.reload().await.unwrap();
    assert!(server.libs.is_empty());
    assert!(old.cancel.is_cancelled() && new.cancel.is_cancelled());
    assert_eq!(server.sandbox.lib_slots.available_permits(), 3);
    drop(permit);
    assert_eq!(server.sandbox.lib_slots.available_permits(), 4);
}

#[tokio::test]
async fn reload_failure_keeps_config_and_lib_and_close_is_permanent() {
    let root = tempfile::tempdir().unwrap();
    let mut server = server(root.path()).await;
    let old = server.libs["echo"].clone();
    let db = rusqlite::Connection::open(server.profile.db_dir().join("config.db")).unwrap();
    db.execute("UPDATE settings SET listen=3", []).unwrap();
    assert!(matches!(
        server.reload().await,
        Err(Error::InvalidConfig(_))
    ));
    assert_eq!(server.config.listen, 0);
    assert!(Arc::ptr_eq(&old, &server.libs["echo"]));
    server.close().await.unwrap();
    assert!(old.cancel.is_cancelled());
    assert!(server.cancel.is_cancelled() && server.sandbox.lib_slots.is_closed());
    assert!(server.libs.is_empty() && server.sandbox.tasks.is_closed());
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
        load_libs(
            &server.profile,
            &server.runtime,
            &server.libs,
            &server.cancel
        )
        .is_err()
    );
}
