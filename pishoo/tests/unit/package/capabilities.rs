use serde_json::json;
use tower::ServiceExt;

use super::*;

#[tokio::test]
async fn access_groups_register_mounted_operations_and_populate_catalog() {
    for publisher in ["std.dhttp.net", "alice.dhttp.net"] {
        for legacy in [false, true] {
            for app in [false, true] {
                let title = if legacy {
                    json!("Receive")
                } else {
                    json!({"en":"Receive","zh-CN":"接收消息"})
                };
                let label = if legacy {
                    "Receive"
                } else {
                    "接收消息 / Receive"
                };
                let root = tempfile::tempdir().unwrap();
                let profile = IdentityProfile::try_from(root.path().join("owner")).unwrap();
                crate::configure(&profile, None, true).unwrap();
                let mut db = crate::setup::open_config(
                    &profile,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
                )
                .unwrap();
                let mut document = json!({"openapi":"3.1.0","info":{"title":"Message","version":"1.0.0","description":"test","license":{"name":"MIT"}},
                "x-pishoo":{"kind":"lib","publisher":publisher,"name":"message","dependencies":[]},
                "x-access-groups":{"message":{"title":title,"operations":["receive"],"app":app}},
                "paths":{"/message":{"post":{"operationId":"receive","summary":"Receive messages","x-access-group":"message","responses":{"200":{"description":"ok"}}}}}});
                if legacy {
                    document["x-pishoo"]["access-groups"] = document
                        .as_object_mut()
                        .unwrap()
                        .remove("x-access-groups")
                        .unwrap();
                    document["paths"]["/message"]["post"]["x-pishoo"] =
                        json!({"access-group":"message"});
                    document["paths"]["/message"]["post"]
                        .as_object_mut()
                        .unwrap()
                        .remove("x-access-group");
                }
                let bytes = crate::package::tests::package_fixture(
                    &document,
                    include_bytes!("../../fixtures/wasi-http-read-request-then-respond.wasm"),
                    if app {
                        std::collections::BTreeMap::from([(
                            "index.html".parse().unwrap(),
                            b"hello".to_vec().into(),
                        )])
                    } else {
                        std::collections::BTreeMap::new()
                    },
                    &crate::package::tests::publisher(publisher, 'a'),
                );
                let package = read_package(&bytes).unwrap();
                let metadata = package_metadata(&package).unwrap();
                let tx = db.transaction().unwrap();
                save_registration(&tx, &format!("{publisher}/message"), &metadata, &package)
                    .unwrap();
                tx.commit().unwrap();
                let path = if publisher == "std.dhttp.net" {
                    "/std/message"
                } else {
                    "/message/message"
                };
                assert_eq!(
                    db.query_row(
                        "SELECT capability_id,title FROM lib_capability",
                        [],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    )
                    .unwrap(),
                    (
                        "message".into(),
                        title
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| title.to_string())
                    )
                );
                assert_eq!(
                    db.query_row(
                        "SELECT method,path FROM lib_capability_operation WHERE method='POST'",
                        [],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    )
                    .unwrap(),
                    ("POST".into(), path.into())
                );
                let response = crate::routes::capabilities_router(profile.clone())
                    .oneshot(
                        http::Request::builder()
                            .uri("/std/acl/permissions")
                            .body(axum::body::Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), http::StatusCode::OK);
                assert_eq!(response.headers()[http::header::CACHE_CONTROL], "no-store");
                let body = axum::body::to_bytes(response.into_body(), 65536)
                    .await
                    .unwrap();
                let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let mut expected = json!({path:["POST"]});
                if app {
                    expected["/app/message"] = json!(["GET"]);
                }
                assert_eq!(
                    value,
                    json!({format!("{publisher}/message · {label}"):expected})
                );
                let response = crate::routes::capabilities_router(profile)
                    .oneshot(
                        http::Request::builder()
                            .uri("/std/acl/permissions?details=1")
                            .body(axum::body::Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), http::StatusCode::OK);
                let body = axum::body::to_bytes(response.into_body(), 65536)
                    .await
                    .unwrap();
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
                    json!({format!("{publisher}/message · {label}"):{"title":title,"endpoints":expected}})
                );
            }
        }
    }
}

#[test]
fn app_capability_rejects_missing_frontend_and_untrusted_targets() {
    for author in ["std.dhttp.net", "alice.dhttp.net"] {
        for (app, path, has_app) in [
            (json!(true), "/message", false),
            (json!("/app/workspace"), "/message", true),
            (json!({"path":"/std/acl"}), "/message", true),
            (json!(true), "/app/workspace", true),
            (json!(true), "/app/message", true),
            (json!(true), "/app/message/../workspace", true),
        ] {
            let document = json!({"openapi":"3.1.0","info":{"title":"Test","version":"1.0.0","description":"test","license":{"name":"MIT"}},
                "x-pishoo":{"kind":"lib","publisher":author,"name":"message","dependencies":[]},
                "x-access-groups":{"read":{"title":"Read","operations":["read"],"app":app}},
                "paths":{path:{"get":{"operationId":"read","summary":"Read","x-access-group":"read","responses":{"200":{"description":"ok"}}}}}});
            let public = if has_app {
                std::collections::BTreeMap::from([(
                    "index.html".parse().unwrap(),
                    b"hello".to_vec().into(),
                )])
            } else {
                std::collections::BTreeMap::new()
            };
            let bytes = crate::package::tests::package_fixture(
                &document,
                include_bytes!("../../fixtures/wasi-http-read-request-then-respond.wasm"),
                public,
                &crate::package::tests::publisher(author, 'a'),
            );
            assert!(
                read_package(&bytes).is_err(),
                "accepted {author} {app} {path}"
            );
        }
    }
}
