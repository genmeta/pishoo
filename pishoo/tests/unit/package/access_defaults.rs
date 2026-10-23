use super::*;
use crate::package::tests::packages::{
    frontend_document, frontend_files, package_fixture, publisher,
};
use http::Method;
use serde_json::json;
use webc::v3::write::Directory;

#[tokio::test]
async fn app_upgrades_preserve_root_rules_when_asset_names_change() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("owner")).unwrap();
    std::fs::create_dir_all(profile.join(".volume/sys/access")).unwrap();
    let access = access_control::AccessService::load_from_db(
        &format!(
            "sqlite://{}?mode=rwc",
            profile.join(".volume/sys/access/database").display()
        ),
        profile.name(),
        &access_control::SubjectId::new(b"owner").unwrap(),
    )
    .await
    .unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let authority = publisher("alice.dhttp.net", 'a');
    let bytes = package_fixture(
        &frontend_document("1.0.0"),
        &[],
        frontend_files("old page"),
        &authority,
    );
    install_app(&profile, "alice.dhttp.net/hello", &bytes).unwrap();
    for (version, effect, expected) in [
        ("2.0.0", access_control::Effect::Allow, "allow"),
        ("3.0.0", access_control::Effect::Deny, "deny"),
    ] {
        access
            .set_policy(
                access_control::Method::Specified(Method::GET),
                "/app/hello",
                effect,
                access_control::Grantee::All,
            )
            .await
            .unwrap();
        let mut files = frontend_files("new page");
        files.children.insert(
            format!("chunk-{version}.js").parse().unwrap(),
            b"new asset".to_vec().into(),
        );
        let bytes = package_fixture(&frontend_document(version), &[], files, &authority);
        let installed = install_app(&profile, "alice.dhttp.net/hello", &bytes).unwrap();
        assert_eq!(installed["version"], version);
        let db = rusqlite::Connection::open(profile.join(".volume/sys/access/database")).unwrap();
        let rules: Vec<String> = db
            .prepare("SELECT effect FROM access_rules")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(rules, [expected]);
    }
}

#[tokio::test]
async fn business_read_rules_have_no_implicit_provider_exemptions() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("owner")).unwrap();
    std::fs::create_dir_all(profile.join(".volume/sys/access")).unwrap();
    let access = access_control::AccessService::load_from_db(
        &format!(
            "sqlite://{}?mode=rwc",
            profile.join(".volume/sys/access/database").display()
        ),
        profile.name(),
        &access_control::SubjectId::new(b"owner").unwrap(),
    )
    .await
    .unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let conn =
        crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    let authority = publisher("std.dhttp.net", 'a');
    for name in ["introduction", "workspace"] {
        let mut doc = frontend_document("1.0.0");
        doc["x-pishoo"]["publisher"] = "std.dhttp.net".into();
        doc["x-pishoo"]["name"] = name.into();
        let bytes = package_fixture(&doc, &[], frontend_files("page"), &authority);
        let package = read_package(&bytes).unwrap();
        check_existing_rules(
            &profile,
            &conn,
            &format!("std.dhttp.net/{name}"),
            "app",
            &package,
            false,
        )
        .unwrap();
        for effect in [access_control::Effect::Allow, access_control::Effect::Deny] {
            access
                .set_policy(
                    access_control::Method::Specified(Method::GET),
                    &format!("/app/{name}"),
                    effect,
                    access_control::Grantee::All,
                )
                .await
                .unwrap();
            for author in ["std.dhttp.net", "other.example"] {
                assert!(
                    check_existing_rules(
                        &profile,
                        &conn,
                        &format!("{author}/{name}"),
                        "app",
                        &package,
                        false,
                    )
                    .is_err(),
                    "{author}/{name} must not inherit an existing {effect:?} rule"
                );
            }
        }
        access
            .remove_policy(
                access_control::Method::Specified(Method::GET),
                &format!("/app/{name}"),
                access_control::Grantee::All,
            )
            .await
            .unwrap();
    }
    let doc = json!({"openapi":"3.1.0","info":{"title":"Profile","version":"1.0.0","description":"Test","license":{"name":"MIT"}},
        "x-pishoo":{"publisher":"std.dhttp.net","name":"profile"},
        "paths":{"/profile/nickname":{"get":{"operationId":"nickname","summary":"Nickname"}},"/profile/avatar":{"get":{"operationId":"avatar","summary":"Avatar"}},
            "/profile/introduction":{"get":{"operationId":"introduction","summary":"Introduction"}},"/profile/permission/list":{"get":{"operationId":"presets","summary":"Presets"}}}});
    let bytes = package_fixture(
        &doc,
        include_bytes!("../../fixtures/wasi-http-read-request-then-respond.wasm"),
        Directory::default(),
        &authority,
    );
    let package = read_package(&bytes).unwrap();
    check_existing_rules(
        &profile,
        &conn,
        "std.dhttp.net/profile",
        "lib",
        &package,
        false,
    )
    .unwrap();
    for path in [
        "/std/profile/nickname",
        "/std/profile/avatar",
        "/std/profile",
    ] {
        access
            .set_policy(
                access_control::Method::Specified(Method::GET),
                path,
                access_control::Effect::Allow,
                access_control::Grantee::All,
            )
            .await
            .unwrap();
        assert!(
            check_existing_rules(
                &profile,
                &conn,
                "std.dhttp.net/profile",
                "lib",
                &package,
                false,
            )
            .is_err(),
            "Profile must not inherit an existing rule for {path}"
        );
        access
            .remove_policy(
                access_control::Method::Specified(Method::GET),
                path,
                access_control::Grantee::All,
            )
            .await
            .unwrap();
    }
}
