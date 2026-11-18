use access_control::{
    ContactPatch, ContactStatus, Effect, Grantee, Method as AccessMethod, NewContact, SubjectId,
    Visitor,
};
use http::{Method, StatusCode};
use serde_json::json;

use super::*;

fn request(
    local: &dhttp::LocalAuthority,
    remote: Option<&dhttp::LocalAuthority>,
    method: Method,
    path: &str,
    body: &str,
) -> Request<AxumBody> {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(AxumBody::from(body.to_owned()))
        .unwrap();
    if let Some(remote) = remote {
        request.extensions_mut().insert(remote.clone());
    } else {
        request.extensions_mut().insert(dhttp::HandshakeSummary {
            alpn: None,
            local: Some(local.clone()),
            remote: None,
        });
    }
    request
}

#[tokio::test]
async fn identity_management_uses_explicit_path_acl_for_verified_delegates() {
    let root = tempfile::tempdir().unwrap();
    let mut server = server(root.path()).await;
    let local = server.endpoint.local_authority().unwrap();
    let subject = SubjectId::new("a".repeat(64).as_bytes()).unwrap();
    server.access = Arc::new(
        access_control::AccessService::load_from_db("sqlite::memory:", local.name(), &subject)
            .await
            .unwrap(),
    );
    let delegate = crate::test_support::identity::endpoint("delegate.name")
        .local_authority()
        .unwrap();
    let stranger = crate::test_support::identity::endpoint("stranger.name")
        .local_authority()
        .unwrap();
    let now = chrono::Utc::now().timestamp();
    server
        .access
        .create_contact(NewContact {
            name: delegate.name().to_owned(),
            subject_id: subject.clone(),
            class: "Human".into(),
            description: "Explicit management delegate".into(),
            requested_access: Default::default(),
            offers: Default::default(),
            created_at: now,
            updated_at: now,
            expired_after: i64::MAX,
        })
        .await
        .unwrap();
    server
        .access
        .patch_contact(
            delegate.name(),
            ContactPatch::LocalUpdate {
                status: Some(ContactStatus::Active),
                alias: None,
            },
        )
        .await
        .unwrap();
    let app = current_router(&server);
    let operations = [
        (Method::GET, "/std/sys/listen", ""),
        (Method::PATCH, "/std/sys/listen", r#"{"listen":"external"}"#),
        (Method::GET, "/std/sys/mount", ""),
        (
            Method::PATCH,
            "/std/sys/mount",
            r#"{"location":"/delegated","proxy_pass":"127.0.0.1:8080"}"#,
        ),
        (Method::GET, "/std/sys/pkg/list", ""),
        (Method::GET, "/std/sys/pkg?name=echo", ""),
        (Method::POST, "/std/sys/pkg/install", "{}"),
        (Method::DELETE, "/std/sys/pkg?name=echo", ""),
    ];
    for (method, path, body) in &operations {
        assert_eq!(
            app.clone()
                .oneshot(request(&local, Some(&delegate), method.clone(), path, body))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN,
            "{method} {path} without a grant"
        );
    }
    server
        .access
        .set_policy(
            AccessMethod::Specified(Method::GET),
            "/std/sys/listen",
            Effect::Allow,
            Grantee::One(delegate.name().to_owned()),
        )
        .await
        .unwrap();
    assert_eq!(
        app.clone()
            .oneshot(request(
                &local,
                Some(&delegate),
                Method::GET,
                "/std/sys/listen",
                ""
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    for (method, path, body) in &operations[1..] {
        assert_eq!(
            app.clone()
                .oneshot(request(&local, Some(&delegate), method.clone(), path, body))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN,
            "GET listen must not grant {method} {path}"
        );
    }
    for (method, path) in [
        (Method::PATCH, "/std/sys/listen"),
        (Method::GET, "/std/sys/mount"),
        (Method::PATCH, "/std/sys/mount"),
        (Method::GET, "/std/sys/pkg/list"),
        (Method::GET, "/std/sys/pkg"),
        (Method::POST, "/std/sys/pkg/install"),
        (Method::DELETE, "/std/sys/pkg"),
    ] {
        server
            .access
            .set_policy(
                AccessMethod::Specified(method),
                path,
                Effect::Allow,
                Grantee::One(delegate.name().to_owned()),
            )
            .await
            .unwrap();
    }
    for (method, path, body) in &operations[..7] {
        let expected = if *method == Method::POST {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::OK
        };
        assert_eq!(
            app.clone()
                .oneshot(request(&local, Some(&delegate), method.clone(), path, body))
                .await
                .unwrap()
                .status(),
            expected,
            "authorized {method} {path}"
        );
    }
    let config = crate::setup::load_server_config(&server.profile).unwrap();
    assert_eq!(config.listen, "external");
    assert_eq!(config.proxy_locations.len(), 1);
    assert_eq!(config.proxy_locations[0].location, "/delegated");
    for remote in [None, Some(&stranger)] {
        let mut forged = request(&local, remote, Method::GET, "/std/sys/pkg/list", "");
        forged
            .extensions_mut()
            .insert(Visitor::new(local.name(), subject.clone()));
        assert_eq!(
            app.clone().oneshot(forged).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
    }
    server
        .access
        .set_policy(
            AccessMethod::Specified(Method::GET),
            "/std/sys/listen",
            Effect::Deny,
            Grantee::One(delegate.name().to_owned()),
        )
        .await
        .unwrap();
    assert_eq!(
        app.clone()
            .oneshot(request(
                &local,
                Some(&delegate),
                Method::GET,
                "/std/sys/listen",
                ""
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.clone()
            .oneshot(request(
                &local,
                Some(&delegate),
                Method::DELETE,
                "/std/sys/pkg?name=echo",
                ""
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
    assert!(!server.sandbox.read().unwrap().libs.contains_key("echo"));
    assert_eq!(
        crate::package::installed_packages(&server.profile, None).unwrap(),
        json!([])
    );
}
