use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::*;

fn fixture(profile: &IdentityProfile, name: &str, author: &str) -> Value {
    crate::configure(profile, None, true).unwrap();
    let standard = crate::package::is_standard_author(author);
    let path = format!("lib/{}{name}.wasm", if standard { "std/" } else { "" });
    std::fs::create_dir_all(profile.join(&path).parent().unwrap()).unwrap();
    std::fs::write(profile.join(&path), b"old executable").unwrap();
    std::fs::create_dir_all(profile.join(format!("app/{name}/assets"))).unwrap();
    std::fs::write(profile.join(format!("app/{name}/index.html")), b"old page").unwrap();
    std::fs::write(profile.join(format!("app/{name}/assets/a.js")), b"old js").unwrap();
    let db =
        crate::setup::open_config(profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    db.execute("INSERT INTO package(name,author,version,installed_at,upgraded_at,has_lib,has_app) VALUES (?1,?2,'1.0.0',123,456,1,1)", rusqlite::params![name,author]).unwrap();
    let files = [
        path,
        format!("app/{name}/index.html"),
        format!("app/{name}/assets/a.js"),
    ]
    .into_iter()
    .map(|path| {
        let bytes = std::fs::read(profile.join(&path)).unwrap();
        (
            path,
            json!({"sha256":format!("{:x}", Sha256::digest(&bytes)),"size":bytes.len()}),
        )
    })
    .collect::<serde_json::Map<_, _>>();
    json!({"author":author,"name":name,"version":"1.0.0","author_owner_hash":"a".repeat(64),"package_sha256":"b".repeat(64),
        "target_author":if standard {"dev.genmeta.tech.dhttp.net"} else {"dev.alice.dhttp.net"},"target_owner_hash":"c".repeat(64),
        "evidence":"TEST ONLY: audited legacy release","files":files})
}

fn author(profile: &IdentityProfile, name: &str) -> String {
    crate::setup::open_config(profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap()
        .query_row("SELECT author FROM package WHERE name=?1", [name], |r| {
            r.get(0)
        })
        .unwrap()
}

#[test]
fn legacy_transfer_preserves_data_acl_historical_facts_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(dir.path().join("owner")).unwrap();
    let entry = fixture(&profile, "profile", "std.dhttp.net");
    std::fs::create_dir_all(profile.join(".volume/std/profile")).unwrap();
    std::fs::write(
        profile.join(".volume/std/profile/database"),
        b"business data",
    )
    .unwrap();
    let acl = std::fs::read(profile.join(".volume/sys/access/database")).unwrap();
    let groups = json!([{"group":"standard","packages":[entry]}]);
    migrate_legacy_groups(&profile, &groups).unwrap();
    assert_eq!(author(&profile, "profile"), "dev.genmeta.tech.dhttp.net");
    let db =
        crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let facts: (String,i64,i64,Option<String>,Option<String>,String) = db.query_row("SELECT version,installed_at,upgraded_at,package_sha256,signer_leaf_fingerprint_sha256,author_owner_hash FROM package",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).unwrap();
    assert_eq!(
        facts,
        ("1.0.0".into(), 123, 456, None, None, "c".repeat(64))
    );
    assert_eq!(
        std::fs::read(profile.join(".volume/std/profile/database")).unwrap(),
        b"business data"
    );
    assert_eq!(
        std::fs::read(profile.join(".volume/sys/access/database")).unwrap(),
        acl
    );
    let before = std::fs::read(profile.join(".volume/sys/pkg/database")).unwrap();
    migrate_legacy_groups(&profile, &groups).unwrap();
    assert_eq!(
        std::fs::read(profile.join(".volume/sys/pkg/database")).unwrap(),
        before
    );
    // A newer signed version is no longer subject to the historical file hashes.
    drop(db);
    let db =
        crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    db.execute(
        "UPDATE package SET version='2.0.0',signer_leaf_fingerprint_sha256=?1",
        ["d".repeat(64)],
    )
    .unwrap();
    std::fs::write(profile.join("lib/std/profile.wasm"), b"new executable").unwrap();
    migrate_legacy_groups(&profile, &groups).unwrap();
    assert_eq!(author(&profile, "profile"), "dev.genmeta.tech.dhttp.net");
}

#[test]
fn invalid_member_rolls_back_whole_group_without_blocking_other_groups() {
    let dir = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(dir.path().join("owner")).unwrap();
    let profile_entry = fixture(&profile, "profile", "std.dhttp.net");
    let workspace = fixture(&profile, "workspace", "std.dhttp.net");
    let journal = fixture(&profile, "journal", "alice.dhttp.net");
    std::fs::write(profile.join("app/workspace/assets/a.js"), b"tampered").unwrap();
    migrate_legacy_groups(
        &profile,
        &json!([
            {"group":"workspace","packages":[profile_entry,workspace]},
            {"group":"journal","packages":[journal]}
        ]),
    )
    .unwrap();
    assert_eq!(author(&profile, "profile"), "std.dhttp.net");
    assert_eq!(author(&profile, "workspace"), "std.dhttp.net");
    assert_eq!(author(&profile, "journal"), "dev.alice.dhttp.net");
}

#[test]
fn legacy_transfer_rejects_unknown_version_owner_digest_signed_install_and_unsafe_files() {
    for reason in [
        "version",
        "owner",
        "digest",
        "signed",
        "missing",
        "extra",
        "symlink",
        "hardlink",
        "composition",
        "placement",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let profile = IdentityProfile::try_from(dir.path().join("owner")).unwrap();
        let mut entry = fixture(&profile, "profile", "std.dhttp.net");
        let db = crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
            .unwrap();
        match reason {
            "version" => {
                db.execute("UPDATE package SET version='0.9.0'", [])
                    .unwrap();
            }
            "owner" => {
                db.execute("UPDATE package SET author_owner_hash=?1", ["d".repeat(64)])
                    .unwrap();
            }
            "digest" => {
                db.execute("UPDATE package SET package_sha256=?1", ["d".repeat(64)])
                    .unwrap();
            }
            "signed" => {
                db.execute(
                    "UPDATE package SET signer_leaf_fingerprint_sha256=?1",
                    ["d".repeat(64)],
                )
                .unwrap();
            }
            "missing" => {
                std::fs::remove_file(profile.join("app/profile/index.html")).unwrap();
            }
            "extra" => {
                std::fs::write(profile.join("app/profile/extra"), b"extra").unwrap();
            }
            "symlink" | "hardlink" => {
                let target = dir.path().join("outside");
                std::fs::write(&target, b"old page").unwrap();
                let path = profile.join("app/profile/index.html");
                std::fs::remove_file(&path).unwrap();
                if reason == "symlink" {
                    std::os::unix::fs::symlink(target, path).unwrap();
                } else {
                    std::fs::hard_link(target, path).unwrap();
                }
            }
            "composition" => {
                db.execute("UPDATE package SET has_lib=0", []).unwrap();
            }
            "placement" => {
                entry["target_author"] = "dev.alice.dhttp.net".into();
            }
            _ => unreachable!(),
        }
        migrate_legacy_groups(&profile, &json!([{"group":"standard","packages":[entry]}])).unwrap();
        assert_eq!(author(&profile, "profile"), "std.dhttp.net", "{reason}");
    }
}

#[test]
fn new_standard_author_preserves_standard_capability_paths_and_native_reservations() {
    assert!(crate::package::is_standard_author(
        "dev.genmeta.tech.dhttp.net"
    ));
    assert!(!crate::package::is_standard_author("dev.genmeta.dhttp.net"));
    assert!(!crate::package::is_standard_author(
        "dev.tech.genmeta.dhttp.net"
    ));
    assert!(crate::package::is_standard_author("std.dhttp.net"));
    assert!(!crate::package::is_standard_author(
        "dev.attacker.dhttp.net"
    ));
    let dir = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(dir.path().join("owner")).unwrap();
    crate::configure(&profile, None, true).unwrap();
    let root = profile_dir(&profile).unwrap();
    std::fs::create_dir(profile.join("lib")).unwrap();
    let lib = child_dir(&root, std::ffi::OsStr::new("lib")).unwrap();
    resource_parent(&lib, "dev.genmeta.tech.dhttp.net/profile", true).unwrap();
    assert!(profile.join("lib/std").is_dir());
}
