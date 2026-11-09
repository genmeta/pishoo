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
        include_bytes!("../../../fixtures/wasi-http-read-request-then-respond.wasm"),
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
    // Reload the installed registry and runtime resources from the same identity.
    load_server_config(&profile).unwrap();
    let mut reloaded = Sandbox::new(Arc::new(runtime));
    reloaded.load_libs(&profile).unwrap();
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
    db.execute_batch(include_str!("../../../fixtures/package/v2/schema.sql"))
        .unwrap();
    db.execute_batch("INSERT INTO lib_registry.lib(name,author,version,title,description,license) VALUES('hello','alice.dhttp.net','1.0.0','Hello','Description','MIT'); INSERT INTO app_registry.app SELECT * FROM lib_registry.lib;
        INSERT INTO app_registry.app_dependency VALUES('hello','hello','1.0.0'); DROP TABLE pkg_registry.package_dependency;DROP TABLE pkg_registry.package;PRAGMA main.user_version=2;").unwrap();
    if conflict {
        db.execute("UPDATE app_registry.app SET author='bob.dhttp.net'", [])
            .unwrap();
    }
}

#[tokio::test]
async fn split_v2_registries_are_preserved_but_not_imported_into_fresh_configuration() {
    for conflict in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let profile = identity(root.path()).await;
        split_v2(&profile, conflict);
        let setting = std::fs::read(profile.join(".volume/sys/setting/database")).unwrap();
        let paths = ["lib", "app"].map(|name| profile.join(format!(".volume/sys/{name}/database")));
        let before = paths
            .iter()
            .map(|path| std::fs::read(path).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(load_server_config(&profile).unwrap().listen, "off");
        for (path, bytes) in paths.iter().zip(before) {
            assert_eq!(std::fs::read(path).unwrap(), bytes);
        }
        let archive = std::fs::read_dir(profile.join(".volume/sys"))
            .unwrap()
            .map(|entry| entry.unwrap())
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".setting-v2-")
            })
            .unwrap();
        assert_eq!(
            std::fs::read(archive.path().join("setting/database")).unwrap(),
            setting
        );
        let conn = crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM package", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}

#[tokio::test]
async fn package_recovery_rolls_back_partial_updates_and_cleans_committed_updates() {
    for (committed, partial, signing_evidence) in [
        (false, false, false),
        (false, true, false),
        (true, false, false),
        (false, false, true),
        (false, true, true),
        (true, false, true),
    ] {
        let root = tempfile::tempdir().unwrap();
        let profile = identity(root.path()).await;
        let db = crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
            .unwrap();
        if !signing_evidence {
            db.execute_batch("DROP TABLE package_dependency; DROP TABLE package;")
                .unwrap();
            db.execute_batch(include_str!(
                "../../../../src/package/sql/compat/v3/schema.sql"
            ))
            .unwrap();
            db.execute_batch("PRAGMA pkg_registry.user_version=3;")
                .unwrap();
        }
        db.execute("INSERT INTO package(name,author,version,has_lib,has_app) VALUES('hello','alice.dhttp.net','2.0.0',1,1)", []).unwrap();
        drop(db);
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
                include_str!("../../../../src/package/sql/compat/v3/registration_facts.sql"),
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
async fn package_recovery_matches_v4_signing_facts_for_installs_and_updates() {
    for previous in [false, true] {
        for committed in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let profile = identity(root.path()).await;
            let mut db =
                crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
                    .unwrap();
            if previous {
                // Keep all v3 facts identical: a signing-evidence mismatch alone
                // must still cause rollback of a same-version update.
                db.execute("INSERT INTO package(name,author,version,has_lib,has_app,author_owner_hash,package_sha256,signer_leaf_fingerprint_sha256) VALUES('hello','alice.dhttp.net','2.0.0',1,1,'owner','old-digest','old-signer')", []).unwrap();
            }
            let tx = db.transaction().unwrap();
            tx.execute("INSERT INTO package(name,author,version,has_lib,has_app,author_owner_hash,package_sha256,signer_leaf_fingerprint_sha256) VALUES('hello','alice.dhttp.net','2.0.0',1,1,'owner','new-digest','new-signer') ON CONFLICT(name) DO UPDATE SET package_sha256=excluded.package_sha256,signer_leaf_fingerprint_sha256=excluded.signer_leaf_fingerprint_sha256", []).unwrap();
            let facts: String = tx
                .query_row(
                    include_str!("../../../../src/package/sql/registration_facts.sql"),
                    ["hello"],
                    |r| r.get(0),
                )
                .unwrap();
            if committed {
                tx.commit().unwrap();
            } else {
                tx.rollback().unwrap();
            }
            drop(db);

            // Files have been switched, but the durable journal and any old
            // backups still exist when the process stops.
            let stage = profile.join(".pishoo-test");
            let member = stage.join("hello");
            std::fs::create_dir_all(member.join("new-lib")).unwrap();
            if previous {
                std::fs::write(member.join("old-lib"), b"old-lib").unwrap();
                std::fs::create_dir(member.join("old-app")).unwrap();
                std::fs::write(member.join("old-app/index.html"), b"old-app").unwrap();
            }
            std::fs::write(profile.join("lib/hello.wasm"), b"new-lib").unwrap();
            std::fs::create_dir(profile.join("app/hello")).unwrap();
            std::fs::write(profile.join("app/hello/index.html"), b"new-app").unwrap();
            std::fs::write(
                stage.join(".transaction.json"),
                json!({"packages":[{"id":"alice.dhttp.net/hello","directory":"hello","facts":facts,"resources":[{"kind":"lib","old":previous,"new":true},{"kind":"app","old":previous,"new":true}]}]}).to_string(),
            )
            .unwrap();

            initialize_directories(&profile).unwrap();
            let installed = installed_packages(&profile, None).unwrap();
            if committed || previous {
                assert_eq!(installed[0]["version"], "2.0.0");
                assert_eq!(installed[0]["author_owner_hash"], "owner");
                assert_eq!(
                    installed[0]["package_sha256"],
                    if committed {
                        "new-digest"
                    } else {
                        "old-digest"
                    }
                );
                assert_eq!(
                    installed[0]["signer_leaf_fingerprint_sha256"],
                    if committed {
                        "new-signer"
                    } else {
                        "old-signer"
                    }
                );
                assert_eq!(
                    std::fs::read(profile.join("lib/hello.wasm")).unwrap(),
                    if committed { b"new-lib" } else { b"old-lib" }
                );
                assert_eq!(
                    std::fs::read(profile.join("app/hello/index.html")).unwrap(),
                    if committed { b"new-app" } else { b"old-app" }
                );
            } else {
                assert!(installed.as_array().unwrap().is_empty());
                assert!(!profile.join("lib/hello.wasm").exists());
                assert!(!profile.join("app/hello").exists());
            }
            assert!(!stage.exists());
            initialize_directories(&profile).unwrap();
            assert_eq!(installed_packages(&profile, None).unwrap(), installed);
        }
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
                include_str!("../../../../src/package/sql/compat/v3/registration_facts.sql"),
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

#[tokio::test]
async fn package_recovery_restores_versioned_and_legacy_filenames_after_interruption() {
    for old_leaf in ["hello.wasm", "hello_1.0.0.wasm"] {
        for phase in 0..=3 {
            let root = tempfile::tempdir().unwrap();
            let profile = identity(root.path()).await;
            let stage = profile.join(".pishoo-versioned-test");
            std::fs::create_dir_all(stage.join("new-lib")).unwrap();
            let new_leaf = "hello_2.0.0.wasm";
            let old = profile.join(format!("lib/{old_leaf}"));
            let new = profile.join(format!("lib/{new_leaf}"));
            let candidate = stage.join("new-lib").join(new_leaf);
            std::fs::write(&old, b"old-lib").unwrap();
            std::fs::write(&candidate, b"new-lib").unwrap();
            let db =
                crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
                    .unwrap();
            db.execute("INSERT INTO package(name,author,version,has_lib,has_app) VALUES('hello','alice.dhttp.net',?1,1,0)", [if phase == 3 { "2.0.0" } else { "1.0.0" }]).unwrap();
            let facts: String = db
                .query_row(
                    include_str!("../../../../src/package/sql/registration_facts.sql"),
                    ["hello"],
                    |r| r.get(0),
                )
                .unwrap();
            drop(db);
            if phase >= 1 {
                std::fs::rename(&old, stage.join("old-lib")).unwrap();
            }
            if phase >= 2 {
                std::fs::rename(&candidate, &new).unwrap();
            }
            let journal = json!({"id":"alice.dhttp.net/hello","facts":if phase == 3 {facts} else {"uncommitted".into()},"resources":[
                {"kind":"lib","old":true,"new":true,"old_leaf":old_leaf,"new_leaf":new_leaf},
                {"kind":"app","old":false,"new":false,"old_leaf":"hello","new_leaf":"hello"}
            ]});
            std::fs::write(stage.join(".transaction.json"), journal.to_string()).unwrap();
            initialize_directories(&profile).unwrap();
            if phase == 3 {
                assert_eq!(std::fs::read(&new).unwrap(), b"new-lib");
                assert!(!old.exists());
            } else {
                assert_eq!(std::fs::read(&old).unwrap(), b"old-lib");
                assert!(!new.exists());
            }
            assert!(!stage.exists());
        }
    }
}
