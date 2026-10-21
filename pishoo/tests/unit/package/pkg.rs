use super::*;
use serde_json::json;
use webc::v3::write::Directory;

fn package(version: &str, lib: bool, app: bool) -> Vec<u8> {
    let doc = json!({"openapi":"3.1.0","info":{"title":"Calendar","version":version,"description":"Combined package","license":{"name":"MIT"}},
        "x-pishoo":{"kind":if lib {"lib"} else {"app"},"publisher":"alice.dhttp.net","name":"calendar","host":{},"dependencies":[]},
        "paths":{"/events":{"post":{"operationId":"events","summary":"Events","responses":{"200":{"description":"ok"}}}}}});
    package_fixture(
        &doc,
        include_bytes!("../../fixtures/wasi-http-read-request-then-respond.wasm"),
        if app {
            Directory::new(
                std::collections::BTreeMap::from([(
                    "index.html".parse().unwrap(),
                    version.as_bytes().to_vec().into(),
                )]),
                webc::v3::Timestamps::default(),
            )
        } else {
            Directory::default()
        },
        &publisher("alice.dhttp.net", 'a'),
    )
}

#[test]
fn unified_package_installs_updates_removes_resources_and_preserves_data() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("owner")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let runtime = std::sync::Arc::new(WasmRuntime::new().unwrap());
    let id = "alice.dhttp.net/calendar";
    install_package(&profile, id, &package("1.0.0", true, true), &runtime).unwrap();
    let metadata = installed_packages(&profile, Some(id)).unwrap();
    assert_eq!(metadata["has_lib"], true);
    assert_eq!(metadata["has_app"], true);
    assert!(metadata.get("kind").is_none());
    assert!(profile.join("lib/calendar.wasm").is_file());
    assert_eq!(
        std::fs::read(profile.join("app/calendar/index.html")).unwrap(),
        b"1.0.0"
    );
    let mut sandbox = Sandbox::new(runtime.clone());
    sandbox.load_libs(&profile).unwrap();
    assert!(sandbox.libs.contains_key(id));
    assert!(sandbox.apps.contains_key(id));
    std::fs::write(profile.join(".volume/calendar/.db"), b"data").unwrap();
    install_package(&profile, id, &package("2.0.0", true, false), &runtime).unwrap();
    assert!(!profile.join("app/calendar").exists());
    sandbox.load_libs(&profile).unwrap();
    assert!(!sandbox.apps.contains_key(id));
    install_package(&profile, id, &package("3.0.0", false, true), &runtime).unwrap();
    assert!(!profile.join("lib/calendar.wasm").exists());
    sandbox.load_libs(&profile).unwrap();
    assert!(!sandbox.libs.contains_key(id));
    assert!(sandbox.apps.contains_key(id));
    remove_package(&profile, id).unwrap();
    assert!(!profile.join("app/calendar").exists());
    assert_eq!(
        std::fs::read(profile.join(".volume/calendar/.db")).unwrap(),
        b"data"
    );
    assert!(
        installed_packages(&profile, None)
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn failed_commit_restores_both_runtime_parts_and_registration() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("owner")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let runtime = WasmRuntime::new().unwrap();
    let id = "alice.dhttp.net/calendar";
    install_package(&profile, id, &package("1.0.0", true, true), &runtime).unwrap();
    let previous = std::fs::read(profile.join("lib/calendar.wasm")).unwrap();
    let db =
        crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    db.execute_batch("CREATE TABLE pkg_registry.fail_commit(name TEXT REFERENCES package(name) DEFERRABLE INITIALLY DEFERRED); CREATE TRIGGER pkg_registry.reject_upgrade AFTER UPDATE ON package BEGIN INSERT INTO fail_commit VALUES('missing'); END;").unwrap();
    assert!(install_package(&profile, id, &package("2.0.0", true, true), &runtime).is_err());
    assert_eq!(
        std::fs::read(profile.join("lib/calendar.wasm")).unwrap(),
        previous
    );
    assert_eq!(
        std::fs::read(profile.join("app/calendar/index.html")).unwrap(),
        b"1.0.0"
    );
    assert_eq!(
        installed_packages(&profile, Some(id)).unwrap()["version"],
        "1.0.0"
    );
}
