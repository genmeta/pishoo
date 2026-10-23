use access_control::{AuthResult, Headers};
use dhttp_home::identity::IdentityProfile;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::*;

async fn call(
    router: &Router,
    local: &dhttp::LocalAuthority,
    method: &str,
    uri: &str,
    body: Option<Value>,
    owner: bool,
) -> axum::response::Response {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(AxumBody::from(
            body.map_or_else(String::new, |body| body.to_string()),
        ))
        .unwrap();
    if owner {
        request.extensions_mut().insert(local.clone());
    } else {
        request.extensions_mut().insert(dhttp::HandshakeSummary {
            alpn: None,
            local: Some(local.clone()),
            remote: None,
        });
        // The authorization layer must discard this forged owner identity.
        let ski =
            dhttp_home::certificate::extract_dhttp_subject_key_identifier(local.certificates())
                .unwrap();
        request.extensions_mut().insert(Visitor::new(
            local.name(),
            SubjectId::new(ski.owner_hash().as_str().as_bytes()).unwrap(),
        ));
    }
    router.clone().oneshot(request).await.unwrap()
}

async fn catalog(router: &Router, local: &dhttp::LocalAuthority) -> Value {
    let response = call(router, local, "GET", "/std/acl/permissions", None, true).await;
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn permission_declarations_follow_install_replace_delete_and_uninstall() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("owner.dhttp.net")).unwrap();
    std::fs::create_dir_all(profile.path()).unwrap();
    std::fs::create_dir_all(profile.join(".volume/sys/access")).unwrap();
    let endpoint = crate::test_identity::endpoint(profile.name());
    let local = endpoint.local_authority().unwrap();
    let ski = dhttp_home::certificate::extract_dhttp_subject_key_identifier(local.certificates())
        .unwrap();
    let subject = SubjectId::new(ski.owner_hash().as_str().as_bytes()).unwrap();
    let access = Arc::new(
        AccessService::load_from_db(
            &format!(
                "sqlite://{}?mode=rwc",
                profile.join(".volume/sys/access/database").display()
            ),
            local.name(),
            &subject,
        )
        .await
        .unwrap(),
    );
    crate::setup::load_server_config(&profile).unwrap();
    let runtime = crate::WasmRuntime::new().unwrap();
    let mut packages = Vec::new();
    for (publisher, name, path, group) in [
        ("std.dhttp.net", "message", "/message", "receive"),
        ("alice.dhttp.net", "notes", "/entries", "read"),
    ] {
        let method = if name == "message" { "put" } else { "get" };
        let document = json!({"openapi":"3.1.0","info":{"title":"Test","version":"1.0.0","description":"test","license":{"name":"MIT"}},
            "x-pishoo":{"kind":"lib","publisher":publisher,"name":name,"dependencies":[]},
            "x-access-groups":{group:{"title":"Original","operations":["operation"]}},
            "paths":{path:{method:{"operationId":"operation","summary":"Test","x-access-group":group,"responses":{"200":{"description":"ok"}}}}}});
        let bytes = crate::package::tests::package_fixture(
            &document,
            include_bytes!("../../fixtures/wasi-http-read-request-then-respond.wasm"),
            webc::v3::write::Directory::default(),
            &crate::package::tests::publisher(publisher, 'a'),
        );
        let id = format!("{publisher}/{name}");
        crate::package::install_package(&profile, &id, &bytes, &runtime).unwrap();
        packages.push((id, bytes));
    }
    let router = capabilities_router(profile.clone()).layer(axum::middleware::from_fn_with_state(
        access.clone(),
        authorize,
    ));
    let original = catalog(&router, &local).await;
    assert_eq!(
        original,
        json!({
            "std.dhttp.net/message · Original":{"/std/message":["PUT"]},
            "alice.dhttp.net/notes · Original":{"/notes/entries":["GET"]}
        })
    );
    let replacement = json!([{"id":"message","title":"Receive","endpoints":[{"method":"PUT","path":"/std/message"}]}]);
    for method in ["PUT", "DELETE"] {
        let body = (method == "PUT").then(|| replacement.clone());
        let response = call(
            &router,
            &local,
            method,
            "/std/acl/permissions?name=message",
            body.clone(),
            false,
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        // Even explicitly permitting anonymous writes cannot bypass the owner check.
        access
            .set_policy(
                access_control::Method::Specified(method.parse().unwrap()),
                "/std/acl/permissions",
                access_control::Effect::Allow,
                access_control::Grantee::All,
            )
            .await
            .unwrap();
        let response = call(
            &router,
            &local,
            method,
            "/std/acl/permissions?name=message",
            body,
            false,
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    for (uri, body, status) in [
        (
            "/std/acl/permissions",
            replacement.clone(),
            StatusCode::BAD_REQUEST,
        ),
        (
            "/std/acl/permissions?name=message&name=notes",
            replacement.clone(),
            StatusCode::BAD_REQUEST,
        ),
        (
            "/std/acl/permissions?name=missing",
            replacement.clone(),
            StatusCode::NOT_FOUND,
        ),
        (
            "/std/acl/permissions?name=message",
            json!({}),
            StatusCode::BAD_REQUEST,
        ),
        (
            "/std/acl/permissions?name=message",
            json!([{"id":"message","title":"Wrong","endpoints":[{"method":"PUT","path":"/std/contact"}]}]),
            StatusCode::BAD_REQUEST,
        ),
        (
            "/std/acl/permissions?name=message",
            json!([{"id":"message","title":"Wrong","endpoints":[{"method":"GET","path":"/std/message"}]}]),
            StatusCode::BAD_REQUEST,
        ),
        (
            "/std/acl/permissions?name=message",
            json!([{"id":"message","title":"Old method","endpoints":[{"method":"POST","path":"/std/message"}]}]),
            StatusCode::BAD_REQUEST,
        ),
        (
            "/std/acl/permissions?name=message",
            json!([replacement[0], replacement[0]]),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let response = call(&router, &local, "PUT", uri, Some(body), true).await;
        assert_eq!(response.status(), status);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(catalog(&router, &local).await, original);
    }
    access
        .set_policy(
            access_control::Method::Specified(Method::PUT),
            "/std/message",
            access_control::Effect::Allow,
            access_control::Grantee::All,
        )
        .await
        .unwrap();
    // A storage failure after deleting the old declarations must roll back
    // both tables, rather than exposing a partial or empty catalog.
    let db =
        crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    db.execute_batch("CREATE TRIGGER acl.reject_permissions BEFORE INSERT ON lib_capability_operation BEGIN SELECT RAISE(ABORT,'test failure'); END;").unwrap();
    assert_eq!(
        call(
            &router,
            &local,
            "PUT",
            "/std/acl/permissions?name=message",
            Some(replacement.clone()),
            true
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(catalog(&router, &local).await, original);
    db.execute_batch("DROP TRIGGER acl.reject_permissions;")
        .unwrap();
    drop(db);
    let response = call(
        &router,
        &local,
        "PUT",
        "/std/acl/permissions?name=message",
        Some(replacement.clone()),
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let changed = catalog(&router, &local).await;
    assert!(changed.get("std.dhttp.net/message · Original").is_none());
    assert_eq!(
        changed["std.dhttp.net/message · Receive"],
        json!({"/std/message":["PUT"]})
    );
    assert_eq!(
        changed["alice.dhttp.net/notes · Original"],
        original["alice.dhttp.net/notes · Original"]
    );
    let mut localized = replacement.clone();
    localized[0]["title"] = json!({"en":"Receive","zh-CN":"接收消息"});
    assert_eq!(
        call(
            &router,
            &local,
            "PUT",
            "/std/acl/permissions?name=message",
            Some(localized.clone()),
            true
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        catalog(&router, &local).await["std.dhttp.net/message · 接收消息 / Receive"],
        json!({"/std/message":["PUT"]})
    );
    let response = call(
        &router,
        &local,
        "GET",
        "/std/acl/permissions?details=1",
        None,
        true,
    )
    .await;
    let body = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    let details: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        details["std.dhttp.net/message · 接收消息 / Receive"]["title"],
        localized[0]["title"]
    );
    for title in [
        json!({"en":"Receive"}),
        json!({"en":"Receive","zh-CN":""}),
        json!({"en":true,"zh-CN":"消息"}),
    ] {
        localized[0]["title"] = title;
        assert_eq!(
            call(
                &router,
                &local,
                "PUT",
                "/std/acl/permissions?name=message",
                Some(localized.clone()),
                true
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            catalog(&router, &local).await["std.dhttp.net/message · 接收消息 / Receive"],
            json!({"/std/message":["PUT"]})
        );
    }
    // Reinstallation uses the package's declarations, replacing a manual edit.
    crate::package::install_package(&profile, &packages[0].0, &packages[0].1, &runtime).unwrap();
    assert_eq!(catalog(&router, &local).await, original);
    let ordinary = json!([{"id":"read","title":"Read notes","endpoints":[{"method":"GET","path":"/notes/entries"}]}]);
    assert_eq!(
        call(
            &router,
            &local,
            "PUT",
            "/std/acl/permissions?name=notes",
            Some(ordinary),
            true
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        catalog(&router, &local).await["alice.dhttp.net/notes · Read notes"],
        json!({"/notes/entries":["GET"]})
    );
    crate::package::install_package(&profile, &packages[1].0, &packages[1].1, &runtime).unwrap();
    assert_eq!(
        call(
            &router,
            &local,
            "PUT",
            "/std/acl/permissions?name=message",
            Some(json!([])),
            true
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    assert!(
        catalog(&router, &local)
            .await
            .get("std.dhttp.net/message · Original")
            .is_none()
    );
    for _ in 0..2 {
        assert_eq!(
            call(
                &router,
                &local,
                "DELETE",
                "/std/acl/permissions?name=message",
                None,
                true
            )
            .await
            .status(),
            StatusCode::NO_CONTENT
        );
    }
    let deleted = catalog(&router, &local).await;
    assert_eq!(
        deleted,
        json!({"alice.dhttp.net/notes · Original":{"/notes/entries":["GET"]}})
    );
    assert_eq!(
        call(
            &router,
            &local,
            "PUT",
            "/std/acl/permissions?name=message",
            Some(replacement),
            true
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    crate::package::remove_package(&profile, &packages[0].0).unwrap();
    assert_eq!(catalog(&router, &local).await, deleted);
    assert_eq!(
        call(
            &router,
            &local,
            "DELETE",
            "/std/acl/permissions?name=message",
            None,
            true
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    let db =
        crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM acl.lib_capability_operation WHERE lib_name='message'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert!(matches!(
        access
            .auth(
                Headers {
                    method: Method::PUT,
                    path: "/std/message".into(),
                    fields: http::HeaderMap::new()
                },
                None,
                None
            )
            .await
            .unwrap(),
        AuthResult::Allowed
    ));
}
