use std::{collections::BTreeMap, sync::Arc};

use serde_json::{Value, json};

use super::*;
fn install_packages(
    profile: &IdentityProfile,
    packages: &[(String, Vec<u8>)],
    sandbox: &mut Sandbox,
) -> crate::Result<()> {
    let verified = packages
        .iter()
        .cloned()
        .map(super::packages::with_proof)
        .collect::<Vec<_>>();
    crate::package::files::install_packages(profile, &verified, sandbox)
}

fn release(
    name: &str,
    version: &str,
    frontend: bool,
    dependencies: &[(&str, &str)],
) -> (String, Vec<u8>) {
    let dependencies = dependencies
        .iter()
        .map(|(name, version)| json!({"name":name,"version":version}))
        .collect::<Vec<_>>();
    let document = json!({
        "openapi":"3.1.0", "info":{"title":name,"version":version,"description":"Dependency update fixture","license":{"name":"MIT"}},
        "x-pishoo":{"kind":if frontend {"app"} else {"lib"},"publisher":"alice.dhttp.net","name":name,"dependencies":dependencies},
        "x-access-groups":{"read":{"title":format!("Read {version}"),"operations":["read"]}},
        "paths":{"/read":{"get":{"operationId":"read","summary":"Read","x-access-group":"read","responses":{"200":{"description":"ok"}}}}}
    });
    let files = if frontend {
        BTreeMap::from([("index.html".parse().unwrap(), version.as_bytes().to_vec())])
    } else {
        std::collections::BTreeMap::new()
    };
    (
        format!("alice.dhttp.net/{name}"),
        package_fixture(
            &document,
            include_bytes!("../../../fixtures/wasi-http-read-request-then-respond.wasm"),
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
        .map(|name| crate::package::registration_snapshot(&db, name, false).unwrap())
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
    let next_releases = releases("2.0.0");
    install_packages(&profile, &next_releases, &mut sandbox).unwrap();
    for name in ["profile", "message"] {
        assert!(profile.join(format!("lib/{name}.wasm.2.0.0")).is_file());
        assert!(profile.join(format!("lib/{name}.wasm.1.0.0")).is_file());
        assert!(!profile.join(format!("lib/{name}.wasm")).exists());
    }
    let catalog = installed_packages(&profile, None).unwrap();
    for package in catalog.as_array().unwrap() {
        let expected = if package["name"] == "workspace" {
            "2.0.0"
        } else {
            "1.0.0"
        };
        assert_eq!(package["version"], expected);
        for dependency in package["dependencies"].as_array().unwrap() {
            assert_eq!(dependency["version"], expected);
        }
        assert_eq!(package["installed_versions"], json!(["1.0.0", "2.0.0"]));
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
        sandbox.versions[&("alice.dhttp.net/profile".to_owned(), "2.0.0".to_owned())]
            .openapi
            .info
            .version,
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
        sandbox.versions[&("alice.dhttp.net/message".to_owned(), "2.0.0".to_owned())]
            .openapi
            .info
            .version,
        "2.0.0"
    );

    let reused = sandbox.libs["alice.dhttp.net/profile"].clone();
    let metadata = installed_packages(&profile, Some("alice.dhttp.net/profile")).unwrap();
    let mut next = next_releases.clone();
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
fn dependency_update_preserves_external_pins_with_parallel_versions() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("owner")).unwrap();
    let mut sandbox = installed(&profile);
    let (id, bytes) = release("other", "1.0.0", true, &[("profile", "1.0.0")]);
    install_package(&profile, &id, &bytes, &sandbox.runtime).unwrap();
    sandbox.load_libs(&profile).unwrap();
    let previous = sandbox.libs["alice.dhttp.net/profile"].clone();
    install_packages(&profile, &releases("2.0.0"), &mut sandbox).unwrap();
    let other = crate::package::installed_version(&profile, &id, "1.0.0").unwrap();
    assert_eq!(other["dependencies"][0]["version"], "1.0.0");
    assert!(Arc::ptr_eq(
        &previous,
        &sandbox.libs["alice.dhttp.net/profile"]
    ));
    assert!(
        sandbox
            .versions
            .contains_key(&("alice.dhttp.net/profile".to_owned(), "2.0.0".to_owned()))
    );
    assert_eq!(
        std::fs::read(profile.join("app/workspace/index.html")).unwrap(),
        b"2.0.0"
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
    let wasm = std::fs::read(profile.join("lib/profile.wasm.1.0.0")).unwrap();
    let db =
        crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    db.execute_batch("CREATE TABLE pkg_registry.fail_commit(name TEXT,version TEXT,FOREIGN KEY(name,version) REFERENCES package(name,version) DEFERRABLE INITIALLY DEFERRED); CREATE TRIGGER pkg_registry.reject_update AFTER INSERT ON package WHEN NEW.name='workspace' AND NEW.version='2.0.0' BEGIN INSERT INTO fail_commit VALUES('missing','0.0.0'); END;").unwrap();
    let mut next = releases("2.0.0");
    next.insert(0, release("extra", "2.0.0", true, &[]));
    assert!(install_packages(&profile, &next, &mut sandbox).is_err());
    for name in ["profile", "message"] {
        assert!(!profile.join(format!("lib/{name}.wasm.2.0.0")).exists());
    }
    assert_eq!(facts(&profile), before);
    assert_eq!(
        std::fs::read(profile.join("lib/profile.wasm.1.0.0")).unwrap(),
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

#[test]
fn legacy_wasm_is_loaded_then_replaced_by_versioned_installation_and_removed() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("owner")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let (id, bytes) = release("message", "1.0.0", false, &[]);
    install_package(&profile, &id, &bytes, &runtime).unwrap();
    std::fs::rename(
        profile.join("lib/message.wasm.1.0.0"),
        profile.join("lib/message.wasm"),
    )
    .unwrap();
    let mut sandbox = Sandbox::new(runtime);
    sandbox.load_libs(&profile).unwrap();
    install_packages(
        &profile,
        &[release("message", "2.0.0", false, &[])],
        &mut sandbox,
    )
    .unwrap();
    assert!(profile.join("lib/message.wasm").is_file());
    assert!(profile.join("lib/message.wasm.2.0.0").is_file());
    sandbox.load_libs(&profile).unwrap();
    assert_eq!(sandbox.libs[&id].openapi.info.version, "2.0.0");
    remove_package(&profile, &id).unwrap();
    assert!(!profile.join("lib/message.wasm.2.0.0").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn one_installation_pins_two_shared_versions_and_reactivates_cached_releases() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("owner")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let mut sandbox = Sandbox::new(Arc::new(WasmRuntime::new().unwrap()));
    let packages = vec![
        release("shared", "1.0.0", false, &[]),
        release("shared", "2.0.0", false, &[]),
        release("a", "1.0.0", false, &[("shared", "1.0.0")]),
        release("b", "1.0.0", false, &[("shared", "2.0.0")]),
        release("root", "1.0.0", false, &[("a", "1.0.0"), ("b", "1.0.0")]),
    ];
    install_packages(&profile, &packages, &mut sandbox).unwrap();
    assert_eq!(sandbox.versions.len(), 5);
    let a = &sandbox.versions[&("alice.dhttp.net/a".to_owned(), "1.0.0".to_owned())];
    let b = &sandbox.versions[&("alice.dhttp.net/b".to_owned(), "1.0.0".to_owned())];
    let old = sandbox.versions[&("alice.dhttp.net/shared".to_owned(), "1.0.0".to_owned())].clone();
    let new = sandbox.versions[&("alice.dhttp.net/shared".to_owned(), "2.0.0".to_owned())].clone();
    assert_eq!(
        a.dependencies["shared"],
        (old.id.clone(), old.openapi.info.version.clone())
    );
    assert_eq!(
        b.dependencies["shared"],
        (new.id.clone(), new.openapi.info.version.clone())
    );
    assert_eq!(
        sandbox.libs["alice.dhttp.net/shared"].openapi.info.version,
        "1.0.0"
    );
    std::fs::write(
        profile.join(".volume/shared/database"),
        b"shared business data",
    )
    .unwrap();
    sandbox.load_libs(&profile).unwrap();
    assert_eq!(sandbox.versions.len(), 5);
    assert_eq!(
        sandbox.libs["alice.dhttp.net/a"].dependencies["shared"].1,
        "1.0.0"
    );
    assert_eq!(
        sandbox.libs["alice.dhttp.net/b"].dependencies["shared"].1,
        "2.0.0"
    );
    let old = sandbox.versions[&("alice.dhttp.net/shared".to_owned(), "1.0.0".to_owned())].clone();
    let new = sandbox.versions[&("alice.dhttp.net/shared".to_owned(), "2.0.0".to_owned())].clone();
    let sandbox = Arc::new(std::sync::RwLock::new(sandbox));
    let endpoint = crate::test_support::identity::endpoint("alice");
    let selected =
        crate::package::install_store_package(&profile, &endpoint, "", "shared", "2.0.0", &sandbox)
            .await
            .unwrap();
    assert_eq!(selected["version"], "2.0.0");
    assert_eq!(
        sandbox.read().unwrap().libs["alice.dhttp.net/shared"]
            .openapi
            .info
            .version,
        "2.0.0"
    );
    assert_eq!(
        sandbox.read().unwrap().libs["alice.dhttp.net/a"].dependencies["shared"].1,
        "1.0.0"
    );
    assert_eq!(
        std::fs::read(profile.join(".volume/shared/database")).unwrap(),
        b"shared business data"
    );
    assert!(!profile.join(".volume/shared.1.0.0").exists());
    assert!(!profile.join(".volume/shared.2.0.0").exists());
    assert!(matches!(
        crate::package::remove_package_version(&profile, "alice.dhttp.net/shared", Some("1.0.0")),
        Err(crate::Error::PackageInUse(_))
    ));
    assert!(matches!(
        crate::package::remove_package_version(&profile, "alice.dhttp.net/shared", Some("2.0.0")),
        Err(crate::Error::PackagePolicy {
            code: "package_active",
            ..
        })
    ));
    assert!(!old.stream_retired.is_cancelled());
    assert!(!new.stream_retired.is_cancelled());
}

#[test]
fn a_source_version_keeps_its_dependency_edges_when_another_version_is_added() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("owner")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let mut sandbox = Sandbox::new(Arc::new(WasmRuntime::new().unwrap()));
    install_packages(
        &profile,
        &[
            release("shared", "1.0.0", false, &[]),
            release("a", "1.0.0", false, &[("shared", "1.0.0")]),
        ],
        &mut sandbox,
    )
    .unwrap();
    install_packages(
        &profile,
        &[
            release("shared", "2.0.0", false, &[]),
            release("a", "2.0.0", false, &[("shared", "2.0.0")]),
        ],
        &mut sandbox,
    )
    .unwrap();
    for version in ["1.0.0", "2.0.0"] {
        let metadata =
            crate::package::installed_version(&profile, "alice.dhttp.net/a", version).unwrap();
        assert_eq!(metadata["dependencies"][0]["version"], version);
        assert_eq!(sandbox.versions[&("alice.dhttp.net/a".to_owned(), version.to_owned())].dependencies["shared"].1, version);
    }
    crate::package::remove_package_version(&profile, "alice.dhttp.net/a", Some("1.0.0")).unwrap();
    assert!(matches!(
        crate::package::remove_package_version(&profile, "alice.dhttp.net/shared", Some("2.0.0")),
        Err(crate::Error::PackageInUse(_))
    ));
    assert!(profile.join("lib/shared.wasm.1.0.0").is_file());
    assert!(profile.join("lib/shared.wasm.2.0.0").is_file());
}

#[tokio::test(flavor = "multi_thread")]
async fn mutual_dependencies_install_reload_route_and_reuse_without_reference_cycles() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("owner")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let mut sandbox = Sandbox::new(Arc::new(WasmRuntime::new().unwrap()));
    let packages = vec![
        release("message", "1.0.0", false, &[("group", "1.0.0")]),
        release("group", "1.0.0", false, &[("message", "1.0.0")]),
    ];
    install_packages(&profile, &packages, &mut sandbox).unwrap();
    for reload in [false, true] {
        if reload {
            sandbox.load_libs(&profile).unwrap();
        }
        let endpoint = crate::test_support::identity::endpoint("alice");
        for (source, target) in [("message", "group"), ("group", "message")] {
            let source_id = format!("alice.dhttp.net/{source}");
            let target_id = format!("alice.dhttp.net/{target}");
            let mut request = http::Request::builder()
                .uri(format!("https://alice.dhttp.net/{target}/read"))
                .body(axum::body::Body::empty())
                .unwrap();
            request
                .extensions_mut()
                .insert(sandbox.libs[&source_id].clone());
            let selected = crate::sandbox::select_library(&sandbox, &endpoint, &request)
                .unwrap()
                .0;
            assert!(Arc::ptr_eq(&selected, &sandbox.libs[&target_id]));
        }
    }
    let weak = Arc::downgrade(&sandbox.libs["alice.dhttp.net/message"]);
    let sandbox = Arc::new(std::sync::RwLock::new(sandbox));
    let endpoint = crate::test_support::identity::endpoint("alice");
    // No store listener exists: a matching installed release must work offline.
    let result =
        crate::package::install_store_package(&profile, &endpoint, "", "group", "1.0.0", &sandbox)
            .await
            .unwrap();
    assert_eq!(result["version"], "1.0.0");
    drop(sandbox);
    assert!(weak.upgrade().is_none());
}
