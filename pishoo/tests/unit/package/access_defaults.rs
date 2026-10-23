use super::*;
use crate::package::tests::packages::{
    frontend_document, frontend_files, package_fixture, publisher,
};
use http::Method;
use serde_json::json;
use std::collections::BTreeMap;
use webc::v3::write::Directory;

#[tokio::test]
async fn configured_read_defaults_allow_only_their_provider_and_exact_rules() {
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
    let defaults: BTreeMap<String, Vec<[String; 4]>> =
        serde_json::from_str(include_str!("../../../src/server/access-defaults.json")).unwrap();
    for [method, path, effect, grantee] in defaults.into_values().flatten() {
        access
            .set_policy(
                method.parse().unwrap(),
                &path,
                effect.parse().unwrap(),
                grantee.parse().unwrap(),
            )
            .await
            .unwrap();
    }
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
        assert!(
            check_existing_rules(
                &profile,
                &conn,
                &format!("other.example/{name}"),
                "app",
                &package,
                false
            )
            .is_err()
        );
    }
    let doc = json!({"openapi":"3.1.0","info":{"title":"Profile","version":"1.0.0","description":"Test","license":{"name":"MIT"}},
        "x-pishoo":{"publisher":"std.dhttp.net","name":"profile"},
        "paths":{"/profile/nickname":{"get":{"operationId":"nickname","summary":"Nickname"}},"/profile/avatar":{"get":{"operationId":"avatar","summary":"Avatar"}},
            "/profile/introduction":{"get":{"operationId":"introduction","summary":"Introduction"}},"/profile/permission-sets":{"get":{"operationId":"presets","summary":"Presets"}}}});
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
    access
        .set_policy(
            access_control::Method::Specified(Method::GET),
            "/std/profile",
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
            false
        )
        .is_err()
    );
    access
        .remove_policy(
            access_control::Method::Specified(Method::GET),
            "/std/profile",
            access_control::Grantee::All,
        )
        .await
        .unwrap();
    access
        .set_policy(
            access_control::Method::Specified(Method::GET),
            "/app/workspace",
            access_control::Effect::Allow,
            access_control::Grantee::All,
        )
        .await
        .unwrap();
    let mut doc = frontend_document("1.0.0");
    doc["x-pishoo"]["publisher"] = "std.dhttp.net".into();
    doc["x-pishoo"]["name"] = "workspace".into();
    let package = read_package(&package_fixture(
        &doc,
        &[],
        frontend_files("page"),
        &authority,
    ))
    .unwrap();
    assert!(
        check_existing_rules(
            &profile,
            &conn,
            "std.dhttp.net/workspace",
            "app",
            &package,
            false
        )
        .is_err()
    );
    // An identity can explicitly configure another provider without Rust changes.
    std::fs::write(
        profile.join(".volume/sys/access/defaults.json"),
        r#"{"other.example/workspace":[["GET","/app/workspace","allow","*?"]]}"#,
    )
    .unwrap();
    check_existing_rules(
        &profile,
        &conn,
        "other.example/workspace",
        "app",
        &package,
        false,
    )
    .unwrap();
    assert!(
        check_existing_rules(
            &profile,
            &conn,
            "std.dhttp.net/workspace",
            "app",
            &package,
            false
        )
        .is_err()
    );
}
