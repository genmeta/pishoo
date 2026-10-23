use http::Method;
use serde_json::json;
use webc::v3::write::Directory;

use super::*;
use crate::package::tests::packages::{
    frontend_document, frontend_files, package_fixture, publisher,
};

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
async fn package_upgrades_replace_apis_and_capabilities_without_changing_or_blocking_acls() {
    for (author, frontend_first) in [
        ("std.dhttp.net", false),
        ("alice.dhttp.net", false),
        ("std.dhttp.net", true),
        ("alice.dhttp.net", true),
    ] {
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
        let mut sandbox =
            crate::sandbox::Sandbox::new(std::sync::Arc::new(WasmRuntime::new().unwrap()));
        let id = format!("{author}/profile");
        let prefix = if author == "std.dhttp.net" {
            "/profile"
        } else {
            ""
        };
        let mount = if author == "std.dhttp.net" {
            "/std/profile"
        } else {
            "/profile"
        };
        let authority = publisher(author, 'a');
        let mut doc = json!({"openapi":"3.1.0","info":{"title":"Profile","version":"1.0.0","description":"API evolution","license":{"name":"MIT"}},
        "x-pishoo":{"kind":if frontend_first {"app"} else {"lib"},"publisher":author,"name":"profile","host":{},"dependencies":[]},
        "x-access-groups":{"read":{"title":"Old read","operations":["nickname"]},"legacy":{"title":"Legacy","operations":["obsolete"]}},
        "paths":{
            format!("{prefix}/nickname"):{"get":{"operationId":"nickname","summary":"Nickname","x-access-group":"read"}},
            format!("{prefix}/obsolete"):{"post":{"operationId":"obsolete","summary":"Obsolete","x-access-group":"legacy"}}
        }});
        let build = |doc: &serde_json::Value| {
            package_fixture(
                doc,
                include_bytes!("../../fixtures/wasi-http-read-request-then-respond.wasm"),
                if doc["x-pishoo"]["kind"] == "app" {
                    frontend_files("Profile")
                } else {
                    Directory::default()
                },
                &authority,
            )
        };
        install_packages(&profile, &[(id.clone(), build(&doc))], &mut sandbox).unwrap();
        // Both broad allow and explicit deny rules must survive; neither is an
        // installation conflict when an existing provider changes its APIs.
        access
            .set_policy(
                access_control::Method::Unspecified,
                mount,
                access_control::Effect::Allow,
                access_control::Grantee::All,
            )
            .await
            .unwrap();
        access
            .set_policy(
                access_control::Method::Specified(Method::GET),
                &format!("{mount}/signature"),
                access_control::Effect::Deny,
                access_control::Grantee::All,
            )
            .await
            .unwrap();
        access
            .set_policy(
                access_control::Method::Specified(Method::POST),
                &format!("{mount}/obsolete"),
                access_control::Effect::Deny,
                access_control::Grantee::Named,
            )
            .await
            .unwrap();
        let db = crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
        let rules = || {
            db.prepare("SELECT method,api,effect,grantee FROM acl.access_rules ORDER BY method,api,effect,grantee").unwrap()
            .query_map([], |r| Ok((r.get::<_, String>(0)?,r.get::<_, String>(1)?,r.get::<_, String>(2)?,r.get::<_, String>(3)?))).unwrap()
            .collect::<rusqlite::Result<Vec<_>>>().unwrap()
        };
        let before = rules();
        doc["info"]["version"] = "2.0.0".into();
        doc["x-pishoo"]["kind"] = "lib".into();
        doc["x-access-groups"] = json!({"manage":{"title":"Manage","operations":["update"]},"public":{"title":"Public fields","operations":["signature"]}});
        doc["paths"] = json!({
            format!("{prefix}/nickname"):{"put":{"operationId":"update","summary":"Update","x-access-group":"manage"}},
            format!("{prefix}/signature"):{"get":{"operationId":"signature","summary":"Signature","x-access-group":"public"}}
        });
        install_packages(&profile, &[(id.clone(), build(&doc))], &mut sandbox).unwrap();
        assert_eq!(rules(), before);
        let operations = db.prepare("SELECT capability_id,method,path FROM lib_capability_operation ORDER BY capability_id").unwrap()
            .query_map([], |r| Ok((r.get::<_, String>(0)?,r.get::<_, String>(1)?,r.get::<_, String>(2)?))).unwrap()
            .collect::<rusqlite::Result<Vec<_>>>().unwrap();
        assert_eq!(
            operations,
            vec![
                ("manage".into(), "PUT".into(), format!("{mount}/nickname")),
                ("public".into(), "GET".into(), format!("{mount}/signature"))
            ]
        );
        let loaded = &sandbox.libs[&id].openapi;
        assert_eq!(loaded.info.version, "2.0.0");
        assert!(
            !loaded
                .paths
                .as_ref()
                .unwrap()
                .contains_key(&format!("{prefix}/obsolete"))
        );

        // Removing all capability declarations must clear the catalogue while
        // retaining the user's path rules, including those for removed APIs.
        doc["info"]["version"] = "3.0.0".into();
        doc.as_object_mut().unwrap().remove("x-access-groups");
        for path in doc["paths"].as_object_mut().unwrap().values_mut() {
            for operation in path.as_object_mut().unwrap().values_mut() {
                operation.as_object_mut().unwrap().remove("x-access-group");
            }
        }
        install_packages(&profile, &[(id.clone(), build(&doc))], &mut sandbox).unwrap();
        assert_eq!(rules(), before);
        for table in ["lib_capability", "lib_capability_operation"] {
            assert_eq!(
                db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                0
            );
        }
        assert_eq!(sandbox.libs[&id].openapi.info.version, "3.0.0");
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
