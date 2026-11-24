use std::{collections::BTreeMap, sync::Arc};

use serde_json::{Value, json};

use super::*;
const AUTHOR: &str = "dev.genmeta.tech.dhttp.net";

fn release(
    name: &str,
    version: &str,
    deps: &[(&str, &str)],
    app: bool,
    owner: char,
) -> (String, Vec<u8>, Value) {
    let document = json!({"openapi":"3.1.0","info":{"title":name,"version":version,"description":"Package rename regression","license":{"name":"MIT"}},
        "x-pishoo":{"kind":if app {"app"} else {"lib"},"publisher":AUTHOR,"name":name,"dependencies":deps.iter().map(|(n,v)|json!({"name":n,"version":v})).collect::<Vec<_>>()},
        "paths":if app {json!({})} else {json!({"/profile":{"get":{"operationId":"read","summary":"Read","responses":{"200":{"description":"ok"}}}}})}});
    let bytes = package_fixture(
        &document,
        if app {
            &[]
        } else {
            include_bytes!("../../../fixtures/wasi-http-read-request-then-respond.wasm")
        },
        if app {
            BTreeMap::from([("index.html".to_owned(), version.as_bytes().to_vec())])
        } else {
            BTreeMap::new()
        },
        &publisher(AUTHOR, owner),
    );
    crate::package::read_package(&bytes).unwrap();
    super::packages::with_proof((format!("{AUTHOR}/{name}"), bytes))
}
fn setup() -> (tempfile::TempDir, IdentityProfile, Sandbox) {
    let temp = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(temp.path().join("owner")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let sandbox = Sandbox::new(Arc::new(WasmRuntime::new().unwrap()));
    (temp, profile, sandbox)
}
fn install(
    profile: &IdentityProfile,
    sandbox: &mut Sandbox,
    releases: &[(String, Vec<u8>, Value)],
) -> crate::Result<()> {
    crate::package::files::install_packages(profile, releases, sandbox)
}
#[test]
fn rename_same_version_replaces_files_and_preserves_business_data() {
    let (_temp, profile, mut sandbox) = setup();
    install(
        &profile,
        &mut sandbox,
        &[release("profile", "1.0.0", &[], false, 'a')],
    )
    .unwrap();
    std::fs::write(profile.join(".volume/std/profile/database"), b"kept data").unwrap();
    let old = sandbox.libs[&format!("{AUTHOR}/profile")].clone();
    install(
        &profile,
        &mut sandbox,
        &[release("std/profile", "1.0.0", &[], false, 'a')],
    )
    .unwrap();
    assert!(old.stream_retired.is_cancelled());
    assert_eq!(
        std::fs::read(profile.join(".volume/std/profile/database")).unwrap(),
        b"kept data"
    );
    assert!(profile.join("lib/std/profile.wasm.1.0.0").is_file());
    let catalog = installed_packages(&profile, None).unwrap();
    assert_eq!(catalog.as_array().unwrap().len(), 1);
    assert_eq!(catalog[0]["name"], "std/profile");
    sandbox.load_libs(&profile).unwrap();
    assert_eq!(sandbox.libs.len(), 1);
}
#[test]
fn rename_requires_active_dependents_in_the_same_transaction() {
    let (_temp, profile, mut sandbox) = setup();
    install(
        &profile,
        &mut sandbox,
        &[
            release("profile", "1.0.0", &[], false, 'a'),
            release("introduction", "1.0.0", &[("profile", "1.0.0")], true, 'a'),
        ],
    )
    .unwrap();
    let before = installed_packages(&profile, None).unwrap();
    assert!(matches!(
        install(
            &profile,
            &mut sandbox,
            &[release("std/profile", "2.0.0", &[], false, 'a')]
        ),
        Err(Error::PackageInUse(_))
    ));
    assert_eq!(installed_packages(&profile, None).unwrap(), before);
    install(
        &profile,
        &mut sandbox,
        &[
            release("std/profile", "2.0.0", &[], false, 'a'),
            release(
                "introduction",
                "2.0.0",
                &[("std/profile", "2.0.0")],
                true,
                'a',
            ),
        ],
    )
    .unwrap();
    let catalog = installed_packages(&profile, None).unwrap();
    assert_eq!(catalog.as_array().unwrap().len(), 2);
    assert!(
        sandbox
            .versions
            .keys()
            .all(|(id, _)| id != &format!("{AUTHOR}/profile"))
    );
    sandbox.load_libs(&profile).unwrap();
    assert!(profile.join("app/introduction.2.0.0/index.html").is_file());
    assert!(!profile.join("lib/std/profile.wasm.1.0.0").exists());
}
#[test]
fn rename_rejects_changed_owner_without_retiring_the_old_instance() {
    let (_temp, profile, mut sandbox) = setup();
    install(
        &profile,
        &mut sandbox,
        &[release("profile", "1.0.0", &[], false, 'a')],
    )
    .unwrap();
    let old = sandbox.libs[&format!("{AUTHOR}/profile")].clone();
    assert!(
        install(
            &profile,
            &mut sandbox,
            &[release("std/profile", "2.0.0", &[], false, 'b')]
        )
        .is_err()
    );
    assert!(!old.stream_retired.is_cancelled());
    assert_eq!(
        installed_packages(&profile, None).unwrap()[0]["name"],
        "profile"
    );
}
#[test]
fn message_volume_move_and_failed_commit_restore_the_original_directory() {
    let (_temp, profile, mut sandbox) = setup();
    install(
        &profile,
        &mut sandbox,
        &[release("message", "1.0.0", &[], false, 'a')],
    )
    .unwrap();
    std::fs::write(
        profile.join(".volume/std/message/database"),
        b"history and attachments",
    )
    .unwrap();
    let db =
        crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    db.execute_batch("CREATE TRIGGER pkg_registry.reject_rename BEFORE INSERT ON package_transaction BEGIN SELECT RAISE(ABORT,'forced rollback'); END;").unwrap();
    let next = release("std/comm/message", "2.0.0", &[], false, 'a');
    assert!(install(&profile, &mut sandbox, &[next.clone()]).is_err());
    assert_eq!(
        std::fs::read(profile.join(".volume/std/message/database")).unwrap(),
        b"history and attachments"
    );
    assert!(!profile.join(".volume/std/comm/message").exists());
    assert_eq!(
        installed_packages(&profile, None).unwrap()[0]["name"],
        "message"
    );
    db.execute_batch("DROP TRIGGER pkg_registry.reject_rename")
        .unwrap();
    install(&profile, &mut sandbox, &[next]).unwrap();
    assert_eq!(
        std::fs::read(profile.join(".volume/std/comm/message/database")).unwrap(),
        b"history and attachments"
    );
    assert!(!profile.join(".volume/std/message").exists());
    sandbox.load_libs(&profile).unwrap();
}
#[test]
fn every_explicit_root_activates_in_one_installation() {
    let (_temp, profile, mut sandbox) = setup();
    let mut releases = vec![
        release("profile", "1.0.0", &[], false, 'a'),
        release("introduction", "1.0.0", &[], true, 'a'),
    ];
    for (_, _, proof) in &mut releases {
        proof["_requested"] = true.into();
    }
    install(&profile, &mut sandbox, &releases).unwrap();
    let mut next = vec![
        release("profile", "2.0.0", &[], false, 'a'),
        release("introduction", "2.0.0", &[], true, 'a'),
    ];
    for (_, _, proof) in &mut next {
        proof["_requested"] = true.into();
    }
    install(&profile, &mut sandbox, &next).unwrap();
    assert!(
        installed_packages(&profile, None)
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["version"] == "2.0.0")
    );
}
#[test]
fn rename_table_is_author_scoped_and_does_not_include_the_legacy_std_author() {
    use crate::package::renames::canonical_name;
    for (old, new) in [
        ("profile", "std/profile"),
        ("message", "std/comm/message"),
        ("comm/message", "std/comm/message"),
        ("comm/group", "std/comm/group"),
        ("calendar", "std/calendar"),
        ("medical", "std/medical"),
        ("command", "std/command"),
        ("python", "std/python"),
    ] {
        assert_eq!(canonical_name(AUTHOR, old), new);
        assert_eq!(canonical_name("std.dhttp.net", old), old);
        assert_eq!(canonical_name("alice.dhttp.net", old), old);
        assert_eq!(canonical_name(AUTHOR, new), new);
    }
}

#[test]
fn interrupted_volume_publication_recovers_before_loading_old_libraries() {
    let (_temp, profile, mut sandbox) = setup();
    install(
        &profile,
        &mut sandbox,
        &[release("message", "1.0.0", &[], false, 'a')],
    )
    .unwrap();
    std::fs::write(
        profile.join(".volume/std/message/database"),
        b"kept history",
    )
    .unwrap();
    let stage = profile.join(".pishoo-12345678901234567890123456789012");
    std::fs::create_dir(&stage).unwrap();
    std::fs::create_dir_all(profile.join(".volume/std/comm")).unwrap();
    let journal = json!({"format":2,"transaction":stage.file_name().unwrap().to_str().unwrap(),"resources":[{
        "id":format!("{AUTHOR}/std/comm/message"),"version":"2.0.0","kind":"volume",
        "old_leaf":"std/message","leaf":"std/comm/message","old":true,"candidate":null,"reuse":true
    }]});
    std::fs::write(
        stage.join(".transaction.json"),
        serde_json::to_vec(&journal).unwrap(),
    )
    .unwrap();
    std::fs::rename(
        profile.join(".volume/std/message"),
        profile.join(".volume/std/comm/message"),
    )
    .unwrap();
    crate::package::recover_version_publication(&profile, &stage, &journal).unwrap();
    assert_eq!(
        std::fs::read(profile.join(".volume/std/message/database")).unwrap(),
        b"kept history"
    );
    assert!(!profile.join(".volume/std/comm/message").exists());
    assert!(!stage.exists());
    sandbox.load_libs(&profile).unwrap();
}

#[test]
fn installation_cannot_reintroduce_a_predecessor_from_its_new_dependency_closure() {
    let (_temp, profile, mut sandbox) = setup();
    assert!(
        install(
            &profile,
            &mut sandbox,
            &[
                release("profile", "1.0.0", &[], false, 'a'),
                release("std/profile", "2.0.0", &[], false, 'a')
            ]
        )
        .is_err()
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
fn cached_dependent_replacement_keeps_its_active_and_explicit_root_registration() {
    let (_temp, profile, mut sandbox) = setup();
    install(
        &profile,
        &mut sandbox,
        &[
            release("profile", "1.0.0", &[], false, 'a'),
            release("introduction", "1.0.0", &[("profile", "1.0.0")], true, 'a'),
        ],
    )
    .unwrap();
    let cached = release("introduction", "2.0.0", &[], true, 'a');
    install(
        &profile,
        &mut sandbox,
        &[
            cached.clone(),
            release("holder", "1.0.0", &[("introduction", "2.0.0")], true, 'a'),
        ],
    )
    .unwrap();
    install(
        &profile,
        &mut sandbox,
        &[cached, release("std/profile", "2.0.0", &[], false, 'a')],
    )
    .unwrap();
    assert_eq!(
        installed_packages(&profile, Some(&format!("{AUTHOR}/introduction"))).unwrap()["version"],
        "2.0.0"
    );
    let db =
        crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT version FROM package_root WHERE name='introduction'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "2.0.0"
    );
    sandbox.load_libs(&profile).unwrap();
}
