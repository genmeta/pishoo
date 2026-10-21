use serde_json::json;

use super::*;
use crate::package::{
    install_app, install_lib, installed_libs, remove_app, remove_lib,
    tests::{
        package_fixture,
        packages::{frontend_document, frontend_files},
        publisher,
    },
};

async fn identity(root: &std::path::Path) -> IdentityProfile {
    let profile = IdentityProfile::try_from(root.join("alice")).unwrap();
    std::fs::create_dir_all(profile.path()).unwrap();
    initialize_directories(&profile).unwrap();
    load_access(&profile, &access_control::SubjectId::new(b"owner").unwrap())
        .await
        .unwrap();
    load_server_config(&profile).unwrap();
    profile
}

fn lib_package(version: &str, method: &str, path: &str) -> Vec<u8> {
    let mut doc = frontend_document(version);
    doc["x-pishoo"]["kind"] = json!("lib");
    doc["x-pishoo"].as_object_mut().unwrap().remove("entry");
    doc["x-pishoo"]["access-groups"] = json!({"write":{"title":"Write","operations":["write"]}});
    doc["paths"] = json!({path:{method:{"operationId":"write","summary":"Write","x-pishoo":{"access-group":"write"},"responses":{"200":{"description":"ok"}}}}});
    package_fixture(
        &doc,
        include_bytes!("../fixtures/wasi-http-read-request-then-respond.wasm"),
        webc::v3::write::Directory::default(),
        &publisher("alice.dhttp.net", 'a'),
    )
}

#[tokio::test]
async fn review_new_layout_dates_dependencies_and_acl_catalog() {
    let root = tempfile::tempdir().unwrap();
    let profile = identity(root.path()).await;
    for old in ["db", "file", "libs", "apps", "logs", "templates"] {
        assert!(!profile.join(old).exists(), "{old}");
    }
    let mut conn =
        crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    for (schema, table) in [
        ("main", "setting"),
        ("lib_registry", "lib"),
        ("app_registry", "app"),
        ("acl", "lib_capability"),
    ] {
        assert!(
            conn.query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM {schema}.sqlite_master WHERE name=?1)"),
                [table],
                |r| r.get::<_, bool>(0)
            )
            .unwrap()
        );
    }
    for schema in ["lib_registry", "app_registry"] {
        assert_eq!(
            conn.query_row(
                &format!(
                    "SELECT count(*) FROM {schema}.pragma_table_info('{}')",
                    if schema == "lib_registry" {
                        "lib"
                    } else {
                        "app"
                    }
                ),
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            9
        );
    }
    assert!(!profile.join(".volume/sys/file").exists());
    assert_eq!(load_server_config(&profile).unwrap().listen, "off");
    crate::setup::config_database(
        &profile,
        &http::Method::PATCH,
        &"/std/sys/listen".parse().unwrap(),
        Some(json!({"listen":"internal"})),
    )
    .unwrap();
    crate::setup::config_database(
        &profile,
        &http::Method::DELETE,
        &"/std/sys/listen".parse().unwrap(),
        None,
    )
    .unwrap();
    for _ in 0..2 {
        assert_eq!(load_server_config(&profile).unwrap().listen, "off");
    }
    assert!(
        !conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM setting WHERE key='listen')",
                [],
                |r| r.get::<_, bool>(0)
            )
            .unwrap()
    );
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let id = "alice.dhttp.net/hello";
    install_lib(
        &profile,
        id,
        &lib_package("1.0.0", "post", "/write"),
        &runtime,
    )
    .unwrap();
    let first = installed_libs(&profile, Some(id)).unwrap();
    assert!(first["installed_at"].as_i64().unwrap() > 0);
    assert!(first["upgraded_at"].is_null());
    assert!(profile.join("lib/hello.wasm").is_file());
    assert_eq!(
        conn.query_row("SELECT path FROM acl.lib_capability_operation", [], |r| {
            r.get::<_, String>(0)
        })
        .unwrap(),
        "/hello/write"
    );
    install_lib(
        &profile,
        id,
        &lib_package("1.0.0", "post", "/write"),
        &runtime,
    )
    .unwrap();
    let reinstalled = installed_libs(&profile, Some(id)).unwrap();
    assert_eq!(first["installed_at"], reinstalled["installed_at"]);
    assert!(reinstalled["upgraded_at"].as_i64().is_some());
    assert!(install_lib(&profile, id, b"invalid", &runtime).is_err());
    assert_eq!(installed_libs(&profile, Some(id)).unwrap(), reinstalled);
    let mut app = frontend_document("1.0.0");
    app["x-pishoo"]["dependencies"] =
        json!([{ "publisher":"alice.dhttp.net","name":"hello","version":"1.0.0" }]);
    let bytes = package_fixture(
        &app,
        &[],
        frontend_files("page"),
        &publisher("alice.dhttp.net", 'a'),
    );
    install_app(&profile, id, &bytes).unwrap();
    assert!(profile.join("app/hello/index.html").is_file());
    assert!(!profile.join("app/hello/public").exists());
    assert!(remove_lib(&profile, id).is_err());
    assert!(
        install_lib(
            &profile,
            id,
            &lib_package("2.0.0", "put", "/updated"),
            &runtime
        )
        .is_err()
    );
    remove_app(&profile, id).unwrap();
    install_lib(
        &profile,
        id,
        &lib_package("2.0.0", "put", "/updated"),
        &runtime,
    )
    .unwrap();
    assert_eq!(
        conn.query_row("SELECT path FROM acl.lib_capability_operation", [], |r| {
            r.get::<_, String>(0)
        })
        .unwrap(),
        "/hello/updated"
    );
    let mut sandbox = Sandbox::new(runtime);
    sandbox.load_libs(&profile).unwrap();
    assert!(!profile.join(".volume/hello/.file").exists());
    std::fs::write(profile.join(".volume/hello/keep"), "data").unwrap();
    remove_lib(&profile, id).unwrap();
    assert_eq!(
        conn.query_row("SELECT count(*) FROM acl.lib_capability", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert!(profile.join(".volume/hello/keep").is_file());
    let tx = conn.transaction().unwrap();
    assert!(
        !tx.prepare("PRAGMA app_registry.foreign_key_check")
            .unwrap()
            .exists([])
            .unwrap()
    );
}

#[tokio::test]
async fn review_old_database_split_is_repeatable_and_preserves_unknown_dates() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.join("db")).unwrap();
    let old = rusqlite::Connection::open(profile.config_db_path()).unwrap();
    old.execute_batch("PRAGMA user_version=1; CREATE TABLE settings(listen TEXT);INSERT INTO settings VALUES('internal');CREATE TABLE proxy_locations(location TEXT,proxy_pass TEXT);INSERT INTO proxy_locations VALUES('/proxy','http://127.0.0.1:8080');
        CREATE TABLE libs(name TEXT PRIMARY KEY,author TEXT,version TEXT,title TEXT,description TEXT,license TEXT,repository TEXT);INSERT INTO libs VALUES('message','std.dhttp.net','1.0.0','Message','Message','Apache-2.0',NULL);
        CREATE TABLE apps(name TEXT PRIMARY KEY,author TEXT,version TEXT);INSERT INTO apps VALUES('workspace','std.dhttp.net','1.0.0');
        CREATE TABLE app_dependencies(app_name TEXT,lib_name TEXT,version TEXT);INSERT INTO app_dependencies VALUES('workspace','message','1.0.0');
        CREATE TABLE lib_dependencies(lib_name TEXT,dependency_name TEXT,version TEXT);
        CREATE TABLE lib_capabilities(lib_name TEXT,capability_id TEXT,title TEXT);INSERT INTO lib_capabilities VALUES('message','receive','Receive');
        CREATE TABLE lib_capability_operations(lib_name TEXT,capability_id TEXT,method TEXT,path TEXT);INSERT INTO lib_capability_operations VALUES('message','receive','POST','/std/message');").unwrap();
    initialize_directories(&profile).unwrap();
    load_access(&profile, &access_control::SubjectId::new(b"owner").unwrap())
        .await
        .unwrap();
    for _ in 0..2 {
        assert_eq!(load_server_config(&profile).unwrap().listen, "internal");
    }
    let db =
        crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM lib", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        db.query_row("SELECT installed_at,upgraded_at FROM lib", [], |r| Ok((
            r.get::<_, Option<i64>>(0)?,
            r.get::<_, Option<i64>>(1)?
        )))
        .unwrap(),
        (None, None)
    );
    assert_eq!(
        db.query_row("SELECT path FROM acl.lib_capability_operation", [], |r| {
            r.get::<_, String>(0)
        })
        .unwrap(),
        "/std/message"
    );
    assert_eq!(
        old.query_row("SELECT count(*) FROM libs", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(db.query_row("SELECT count(*) FROM main.sqlite_master WHERE name IN ('libs','apps','lib_capabilities')",[],|r|r.get::<_,i64>(0)).unwrap(),0);
    assert_eq!(db.query_row("SELECT count(*) FROM app_dependency d JOIN lib l ON l.name=d.lib_name WHERE l.version=d.version",[],|r|r.get::<_,i64>(0)).unwrap(),1);
}

#[tokio::test]
async fn review_recovers_interrupted_install_and_remove_from_committed_facts() {
    let root = tempfile::tempdir().unwrap();
    let profile = identity(root.path()).await;
    let id = "alice.dhttp.net/hello";
    let app = package_fixture(
        &frontend_document("1.0.0"),
        &[],
        frontend_files("old"),
        &publisher("alice.dhttp.net", 'a'),
    );
    install_app(&profile, id, &app).unwrap();
    let target = profile.join("app/hello");
    let interrupted = profile.join(".pishoo-interrupted");
    std::fs::create_dir(&interrupted).unwrap();
    std::fs::write(interrupted.join("index.html"), "uncommitted").unwrap();
    std::fs::rename(&target, interrupted.join(".previous")).unwrap();
    std::fs::write(
        interrupted.join(".transaction.json"),
        json!({"id":id,"kind":"app","action":"install","facts":"uncommitted"}).to_string(),
    )
    .unwrap();
    std::fs::rename(&interrupted, &target).unwrap();
    initialize_directories(&profile).unwrap();
    assert_eq!(std::fs::read(target.join("index.html")).unwrap(), b"old");
    assert!(!target.join(".previous").exists());
    let db =
        crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    let facts: String = db
        .query_row(
            include_str!("../../src/package/registration-facts.sql"),
            ["app", "hello"],
            |r| r.get(0),
        )
        .unwrap();
    std::fs::create_dir(target.join(".previous")).unwrap();
    std::fs::write(target.join(".previous/discard"), "old backup").unwrap();
    std::fs::write(
        target.join(".transaction.json"),
        json!({"id":id,"kind":"app","action":"install","facts":facts}).to_string(),
    )
    .unwrap();
    initialize_directories(&profile).unwrap();
    assert!(!target.join(".previous").exists());
    assert!(!target.join(".transaction.json").exists());
    for committed in [false, true] {
        std::fs::create_dir(&interrupted).unwrap();
        std::fs::rename(&target, interrupted.join("artifact")).unwrap();
        let facts = if committed {
            db.execute("DELETE FROM app WHERE name='hello'", [])
                .unwrap();
            db.query_row(
                include_str!("../../src/package/registration-facts.sql"),
                ["app", "hello"],
                |r| r.get::<_, String>(0),
            )
            .unwrap()
        } else {
            "not committed".to_owned()
        };
        std::fs::write(
            interrupted.join(".transaction.json"),
            json!({"id":id,"kind":"app","action":"remove","facts":facts}).to_string(),
        )
        .unwrap();
        initialize_directories(&profile).unwrap();
        assert_eq!(target.exists(), !committed);
        assert!(!interrupted.exists());
    }
}

#[tokio::test]
async fn review_failed_split_rolls_back_all_databases_and_can_retry() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.join("db")).unwrap();
    let old = rusqlite::Connection::open(profile.config_db_path()).unwrap();
    old.execute_batch("PRAGMA user_version=1; CREATE TABLE settings(listen TEXT);INSERT INTO settings VALUES('internal');CREATE TABLE proxy_locations(location TEXT,proxy_pass TEXT);
        CREATE TABLE libs(name TEXT PRIMARY KEY,author TEXT,version TEXT);INSERT INTO libs VALUES('one','alice.dhttp.net','1.0.0'),('two','alice.dhttp.net','1.0.0');
        CREATE TABLE lib_capabilities(lib_name TEXT,capability_id TEXT,title TEXT);INSERT INTO lib_capabilities VALUES('one','read','Read'),('two','read','Read');
        CREATE TABLE lib_capability_operations(lib_name TEXT,capability_id TEXT,method TEXT,path TEXT);INSERT INTO lib_capability_operations VALUES('one','read','GET','/same'),('two','read','GET','/same');").unwrap();
    initialize_directories(&profile).unwrap();
    load_access(&profile, &access_control::SubjectId::new(b"owner").unwrap())
        .await
        .unwrap();
    for _ in 0..2 {
        assert!(load_server_config(&profile).is_err());
    }
    let db =
        crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM libs", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert!(
        !db.query_row(
            "SELECT EXISTS(SELECT 1 FROM lib_registry.sqlite_master WHERE name='lib')",
            [],
            |r| r.get::<_, bool>(0)
        )
        .unwrap()
    );
    db.execute(
        "UPDATE lib_capability_operations SET path='/different' WHERE lib_name='two'",
        [],
    )
    .unwrap();
    drop(db);
    assert_eq!(load_server_config(&profile).unwrap().listen, "internal");
    let db =
        crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM lib", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn review_flat_lib_files_recover_without_package_directories() {
    let root = tempfile::tempdir().unwrap();
    let profile = identity(root.path()).await;
    let runtime = WasmRuntime::new().unwrap();
    let id = "alice.dhttp.net/hello";
    install_lib(
        &profile,
        id,
        &lib_package("1.0.0", "post", "/write"),
        &runtime,
    )
    .unwrap();
    let target = profile.join("lib/hello.wasm");
    let original = std::fs::read(&target).unwrap();
    let stage = profile.join(".pishoo-flat-test");
    let db =
        crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    for published in [false, true] {
        std::fs::create_dir(&stage).unwrap();
        std::fs::rename(&target, stage.join(".previous")).unwrap();
        std::fs::write(stage.join("hello.wasm"), b"uncommitted").unwrap();
        std::fs::write(
            stage.join(".transaction.json"),
            json!({"id":id,"kind":"lib","action":"install","facts":"not committed"}).to_string(),
        )
        .unwrap();
        if published {
            std::fs::rename(stage.join("hello.wasm"), &target).unwrap();
        }
        initialize_directories(&profile).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), original);
        assert!(!stage.exists());
    }
    let facts: String = db
        .query_row(
            include_str!("../../src/package/registration-facts.sql"),
            ["lib", "hello"],
            |r| r.get(0),
        )
        .unwrap();
    std::fs::create_dir(&stage).unwrap();
    std::fs::write(stage.join(".previous"), b"previous").unwrap();
    std::fs::write(
        stage.join(".transaction.json"),
        json!({"id":id,"kind":"lib","action":"install","facts":facts}).to_string(),
    )
    .unwrap();
    initialize_directories(&profile).unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), original);
    assert!(!stage.exists());
    for committed in [false, true] {
        std::fs::create_dir(&stage).unwrap();
        std::fs::rename(&target, stage.join("artifact")).unwrap();
        let facts = if committed {
            db.execute("DELETE FROM lib_capability WHERE lib_name='hello'", [])
                .unwrap();
            db.execute("DELETE FROM lib WHERE name='hello'", [])
                .unwrap();
            db.query_row(
                include_str!("../../src/package/registration-facts.sql"),
                ["lib", "hello"],
                |r| r.get::<_, String>(0),
            )
            .unwrap()
        } else {
            "not committed".into()
        };
        std::fs::write(
            stage.join(".transaction.json"),
            json!({"id":id,"kind":"lib","action":"remove","facts":facts}).to_string(),
        )
        .unwrap();
        initialize_directories(&profile).unwrap();
        assert_eq!(target.exists(), !committed);
        assert!(!stage.exists());
    }
    assert!(!profile.join("lib/hello").exists());
}
