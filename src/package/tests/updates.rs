use std::{collections::BTreeMap, sync::Arc};

use serde_json::{Value, json};

use super::*;
use crate::package::files::install_packages;

fn release(
    name: &str,
    version: &str,
    frontend: bool,
    dependencies: &[(&str, &str)],
) -> (String, Vec<u8>) {
    let dependencies = dependencies
        .iter()
        .map(|(name, version)| json!({"publisher":"alice.dhttp.net","name":name,"version":version}))
        .collect::<Vec<_>>();
    let document = json!({
        "openapi":"3.1.0", "info":{"title":name,"version":version,"description":"Dependency update fixture","license":{"name":"MIT"}},
        "x-pishoo":{"kind":if frontend {"app"} else {"lib"},"publisher":"alice.dhttp.net","name":name,"dependencies":dependencies},
        "x-access-groups":{"read":{"title":format!("Read {version}"),"operations":["read"]}},
        "paths":{"/read":{"get":{"operationId":"read","summary":"Read","x-access-group":"read","responses":{"200":{"description":"ok"}}}}}
    });
    let files = if frontend {
        BTreeMap::from([(
            "index.html".parse().unwrap(),
            version.as_bytes().to_vec().into(),
        )])
    } else {
        std::collections::BTreeMap::new()
    };
    (
        format!("alice.dhttp.net/{name}"),
        package_fixture(
            &document,
            include_bytes!("../../../tests/fixtures/wasi-http-read-request-then-respond.wasm"),
            files,
            &publisher("alice.dhttp.net", 'a'),
        ),
    )
}

fn releases(version: &str) -> Vec<(String, Vec<u8>)> {
    vec![
        release("profile", version, false, &[]),
        release("message", version, false, &[]),
        release("introduction", version, true, &[("profile", version)]),
        release(
            "workspace",
            version,
            true,
            &[
                ("introduction", version),
                ("profile", version),
                ("message", version),
            ],
        ),
    ]
}

fn installed(profile: &IdentityProfile) -> Sandbox {
    crate::setup::load_server_config(profile).unwrap();
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    for (id, bytes) in releases("1.0.0") {
        install_package(profile, &id, &bytes, &runtime).unwrap();
    }
    let mut sandbox = Sandbox::new(runtime);
    sandbox.load_libs(profile).unwrap();
    std::fs::write(profile.join(".volume/profile/database"), b"profile data").unwrap();
    sandbox
}

fn facts(profile: &IdentityProfile) -> Value {
    let db =
        crate::setup::open_config(profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let rows = db
        .prepare("SELECT name FROM package ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    rows.iter()
        .map(|name| {
            db.query_row(
                include_str!("../sql/registration-facts-v3.sql"),
                [name],
                |r| r.get::<_, String>(0),
            )
            .unwrap()
        })
        .collect::<Vec<_>>()
        .into()
}

#[test]
fn dependency_update_switches_direct_and_shared_transitive_dependencies_together() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("owner")).unwrap();
    let mut sandbox = installed(&profile);
    let previous = sandbox.libs["alice.dhttp.net/profile"].clone();
    let old_page =
        crate::package::app_file(&sandbox.apps["alice.dhttp.net/workspace"].1, "index.html")
            .unwrap();
    install_packages(&profile, &releases("2.0.0"), &mut sandbox).unwrap();
    let catalog = installed_packages(&profile, None).unwrap();
    for package in catalog.as_array().unwrap() {
        assert_eq!(package["version"], "2.0.0");
        for dependency in package["dependencies"].as_array().unwrap() {
            assert_eq!(dependency["version"], "2.0.0");
        }
    }
    assert_eq!(sandbox.libs.len(), 2);
    assert_eq!(sandbox.apps.len(), 2);
    assert_eq!(std::io::read_to_string(old_page).unwrap(), "1.0.0");
    assert_eq!(sandbox.apps["alice.dhttp.net/workspace"].0, "2.0.0");
    assert_eq!(
        std::io::read_to_string(
            crate::package::app_file(&sandbox.apps["alice.dhttp.net/workspace"].1, "index.html")
                .unwrap()
        )
        .unwrap(),
        "2.0.0"
    );
    assert_eq!(
        sandbox.libs["alice.dhttp.net/profile"].openapi.info.version,
        "2.0.0"
    );
    assert_eq!(
        previous.openapi.info.version, "1.0.0",
        "in-flight resources remain valid"
    );
    assert_eq!(
        std::fs::read(profile.join("app/workspace/index.html")).unwrap(),
        b"2.0.0"
    );
    assert_eq!(
        std::fs::read(profile.join(".volume/profile/database")).unwrap(),
        b"profile data"
    );
    let before_restart = facts(&profile);
    sandbox.load_libs(&profile).unwrap();
    assert_eq!(facts(&profile), before_restart);
    assert_eq!(
        sandbox.libs["alice.dhttp.net/message"].openapi.info.version,
        "2.0.0"
    );

    let reused = sandbox.libs["alice.dhttp.net/profile"].clone();
    let metadata = installed_packages(&profile, Some("alice.dhttp.net/profile")).unwrap();
    let mut next = releases("2.0.0");
    next[3] = release(
        "workspace",
        "3.0.0",
        true,
        &[
            ("introduction", "2.0.0"),
            ("profile", "2.0.0"),
            ("message", "2.0.0"),
        ],
    );
    install_packages(&profile, &next, &mut sandbox).unwrap();
    assert!(Arc::ptr_eq(
        &reused,
        &sandbox.libs["alice.dhttp.net/profile"]
    ));
    assert_eq!(
        installed_packages(&profile, Some("alice.dhttp.net/profile")).unwrap(),
        metadata
    );
    assert_eq!(
        std::fs::read(profile.join("app/workspace/index.html")).unwrap(),
        b"3.0.0"
    );
}

#[test]
fn dependency_update_rejects_external_pins_without_changing_installation() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("owner")).unwrap();
    let mut sandbox = installed(&profile);
    let (id, bytes) = release("other", "1.0.0", true, &[("profile", "1.0.0")]);
    install_package(&profile, &id, &bytes, &sandbox.runtime).unwrap();
    sandbox.load_libs(&profile).unwrap();
    let before = facts(&profile);
    let previous = sandbox.libs["alice.dhttp.net/profile"].clone();
    let error = install_packages(&profile, &releases("2.0.0"), &mut sandbox)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("alice.dhttp.net/other requires alice.dhttp.net/profile@1.0.0"),
        "{error}"
    );
    assert_eq!(facts(&profile), before);
    assert!(Arc::ptr_eq(
        &previous,
        &sandbox.libs["alice.dhttp.net/profile"]
    ));
    assert_eq!(
        std::fs::read(profile.join("app/workspace/index.html")).unwrap(),
        b"1.0.0"
    );
}

#[test]
fn dependency_update_rolls_back_all_files_registration_capabilities_and_runtime_on_commit_failure()
{
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("owner")).unwrap();
    let mut sandbox = installed(&profile);
    let before = facts(&profile);
    let previous = sandbox.libs["alice.dhttp.net/profile"].clone();
    let wasm = std::fs::read(profile.join("lib/profile.wasm")).unwrap();
    let db =
        crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    db.execute_batch("CREATE TABLE pkg_registry.fail_commit(name TEXT REFERENCES package(name) DEFERRABLE INITIALLY DEFERRED); CREATE TRIGGER pkg_registry.reject_update AFTER UPDATE ON package WHEN NEW.name='workspace' BEGIN INSERT INTO fail_commit VALUES('missing'); END;").unwrap();
    let mut next = releases("2.0.0");
    next.insert(0, release("extra", "2.0.0", true, &[]));
    assert!(install_packages(&profile, &next, &mut sandbox).is_err());
    assert_eq!(facts(&profile), before);
    assert_eq!(
        std::fs::read(profile.join("lib/profile.wasm")).unwrap(),
        wasm
    );
    for name in ["workspace", "introduction"] {
        assert_eq!(
            std::fs::read(profile.join(format!("app/{name}/index.html"))).unwrap(),
            b"1.0.0"
        );
    }
    assert!(!profile.join("app/extra").exists());
    assert!(!sandbox.apps.contains_key("alice.dhttp.net/extra"));
    assert!(Arc::ptr_eq(
        &previous,
        &sandbox.libs["alice.dhttp.net/profile"]
    ));
    assert_eq!(
        std::fs::read(profile.join(".volume/profile/database")).unwrap(),
        b"profile data"
    );
    assert!(!std::fs::read_dir(profile.path()).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".pishoo-")
    }));
}

#[test]
fn dependency_install_can_fill_missing_packages_and_rejects_inconsistent_graphs() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("owner")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let mut sandbox = Sandbox::new(Arc::new(WasmRuntime::new().unwrap()));
    let mut invalid = releases("2.0.0");
    invalid[3] = release("workspace", "2.0.0", true, &[("profile", "1.0.0")]);
    assert!(install_packages(&profile, &invalid, &mut sandbox).is_err());
    assert_eq!(installed_packages(&profile, None).unwrap(), json!([]));
    assert!(sandbox.libs.is_empty());
    install_packages(&profile, &releases("2.0.0"), &mut sandbox).unwrap();
    assert_eq!(sandbox.libs.len(), 2);
    assert_eq!(sandbox.apps.len(), 2);
}
