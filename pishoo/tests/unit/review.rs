use serde_json::json;

use super::*;
use crate::package::{
    install_package, installed_packages,
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

#[tokio::test]
async fn package_registry_records_both_resources_dates_and_capabilities() {
    let root = tempfile::tempdir().unwrap();
    let profile = identity(root.path()).await;
    let runtime = WasmRuntime::new().unwrap();
    let id = "alice.dhttp.net/hello";
    let mut doc = frontend_document("1.0.0");
    doc["x-pishoo"]["kind"] = "lib".into();
    doc["paths"] = json!({"/events":{"post":{"operationId":"events","summary":"Events"}}});
    let bytes = package_fixture(
        &doc,
        include_bytes!("../fixtures/wasi-http-read-request-then-respond.wasm"),
        frontend_files("page"),
        &publisher("alice.dhttp.net", 'a'),
    );
    install_package(&profile, id, &bytes, &runtime).unwrap();
    let value = installed_packages(&profile, Some(id)).unwrap();
    assert_eq!(value["has_lib"], true);
    assert_eq!(value["has_app"], true);
    assert!(value["installed_at"].as_i64().is_some());
    assert!(value["upgraded_at"].is_null());
    install_package(&profile, id, &bytes, &runtime).unwrap();
    let next = installed_packages(&profile, Some(id)).unwrap();
    assert_eq!(next["installed_at"], value["installed_at"]);
    assert!(next["upgraded_at"].as_i64().is_some());
    assert!(profile.join(".volume/sys/pkg/database").is_file());
    assert!(!profile.join(".volume/sys/lib").exists());
    assert!(!profile.join(".volume/sys/app").exists());
}

fn split_v2(profile: &IdentityProfile, conflict: bool) {
    for name in ["lib", "app"] {
        std::fs::create_dir_all(profile.join(format!(".volume/sys/{name}"))).unwrap();
        std::fs::File::create(profile.join(format!(".volume/sys/{name}/database"))).unwrap();
    }
    let db =
        crate::setup::open_config(profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    for (schema, name) in [("lib_registry", "lib"), ("app_registry", "app")] {
        db.execute(
            &format!("ATTACH DATABASE ?1 AS {schema}"),
            [profile
                .join(format!(".volume/sys/{name}/database"))
                .to_str()
                .unwrap()],
        )
        .unwrap();
    }
    db.execute_batch(include_str!("../../src/package/sql/schema-v2.sql"))
        .unwrap();
    db.execute_batch("INSERT INTO lib_registry.lib(name,author,version,title,description,license) VALUES('hello','alice.dhttp.net','1.0.0','Hello','Description','MIT'); INSERT INTO app_registry.app SELECT * FROM lib_registry.lib;
        INSERT INTO app_registry.app_dependency VALUES('hello','hello','1.0.0'); DROP TABLE pkg_registry.package_dependency;DROP TABLE pkg_registry.package;PRAGMA main.user_version=2;").unwrap();
    if conflict {
        db.execute("UPDATE app_registry.app SET author='bob.dhttp.net'", [])
            .unwrap();
    }
}

#[tokio::test]
async fn compatible_v2_resources_merge_and_internal_dependencies_disappear() {
    let root = tempfile::tempdir().unwrap();
    let profile = identity(root.path()).await;
    split_v2(&profile, false);
    load_server_config(&profile).unwrap();
    let value = installed_packages(&profile, Some("alice.dhttp.net/hello")).unwrap();
    assert_eq!(value["has_lib"], true);
    assert_eq!(value["has_app"], true);
    assert!(value["dependencies"].as_array().unwrap().is_empty());
    assert!(value["installed_at"].is_null());
    load_server_config(&profile).unwrap();
    assert_eq!(
        installed_packages(&profile, None)
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn conflicting_v2_resources_preserve_old_rows_and_can_retry() {
    let root = tempfile::tempdir().unwrap();
    let profile = identity(root.path()).await;
    split_v2(&profile, true);
    assert!(load_server_config(&profile).is_err());
    let db = rusqlite::Connection::open(profile.join(".volume/sys/app/database")).unwrap();
    assert_eq!(
        db.query_row("SELECT author FROM app", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "bob.dhttp.net"
    );
    db.execute("UPDATE app SET author='alice.dhttp.net'", [])
        .unwrap();
    load_server_config(&profile).unwrap();
    assert_eq!(
        installed_packages(&profile, None)
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn package_recovery_rolls_back_partial_updates_and_cleans_committed_updates() {
    for (committed, partial) in [(false, false), (false, true), (true, false)] {
        let root = tempfile::tempdir().unwrap();
        let profile = identity(root.path()).await;
        std::fs::write(profile.join("lib/hello.wasm"), b"old-lib").unwrap();
        std::fs::create_dir(profile.join("app/hello")).unwrap();
        std::fs::write(profile.join("app/hello/index.html"), b"old-app").unwrap();
        let stage = profile.join(".pishoo-test");
        std::fs::create_dir(&stage).unwrap();
        std::fs::create_dir(stage.join("new-lib")).unwrap();
        std::fs::rename(profile.join("lib/hello.wasm"), stage.join("old-lib")).unwrap();
        std::fs::write(profile.join("lib/hello.wasm"), b"new-lib").unwrap();
        if partial {
            std::fs::create_dir(stage.join("new-app")).unwrap();
            std::fs::write(stage.join("new-app/index.html"), b"new-app").unwrap();
        } else {
            std::fs::rename(profile.join("app/hello"), stage.join("old-app")).unwrap();
            std::fs::create_dir(profile.join("app/hello")).unwrap();
            std::fs::write(profile.join("app/hello/index.html"), b"new-app").unwrap();
        }
        let db = crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
        let facts: String = db
            .query_row(
                include_str!("../../src/package/sql/registration-facts-v3.sql"),
                ["hello"],
                |r| r.get(0),
            )
            .unwrap();
        std::fs::write(stage.join(".transaction.json"),json!({"id":"alice.dhttp.net/hello","facts":if committed{facts}else{"uncommitted".into()},"resources":[{"kind":"lib","old":true,"new":true},{"kind":"app","old":true,"new":true}]}).to_string()).unwrap();
        initialize_directories(&profile).unwrap();
        assert_eq!(
            std::fs::read(profile.join("lib/hello.wasm")).unwrap(),
            if committed { b"new-lib" } else { b"old-lib" }
        );
        assert_eq!(
            std::fs::read(profile.join("app/hello/index.html")).unwrap(),
            if committed { b"new-app" } else { b"old-app" }
        );
        assert!(!stage.exists());
    }
}

#[tokio::test]
async fn package_recovery_restores_uncommitted_removal_and_finishes_committed_removal() {
    for committed in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let profile = identity(root.path()).await;
        let stage = profile.join(".pishoo-test");
        std::fs::create_dir(&stage).unwrap();
        std::fs::write(stage.join("old-lib"), b"lib").unwrap();
        std::fs::create_dir(stage.join("old-app")).unwrap();
        std::fs::write(stage.join("old-app/index.html"), b"app").unwrap();
        let db = crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
        let facts: String = db
            .query_row(
                include_str!("../../src/package/sql/registration-facts-v3.sql"),
                ["hello"],
                |r| r.get(0),
            )
            .unwrap();
        std::fs::write(stage.join(".transaction.json"),json!({"id":"alice.dhttp.net/hello","facts":if committed{facts}else{"uncommitted".into()},"resources":[{"kind":"lib","old":true,"new":false},{"kind":"app","old":true,"new":false}]}).to_string()).unwrap();
        initialize_directories(&profile).unwrap();
        assert_eq!(profile.join("lib/hello.wasm").exists(), !committed);
        assert_eq!(profile.join("app/hello/index.html").exists(), !committed);
        assert!(!stage.exists());
    }
}
