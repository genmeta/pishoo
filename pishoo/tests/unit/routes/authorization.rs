use super::*;

#[tokio::test]
async fn app_head_exposes_verified_viewer_without_a_new_endpoint() {
    use access_control::{Effect, Grantee};
    use tower::ServiceExt;

    let (access, _) = authorization_app(Effect::Deny).await;
    access
        .set_policy(
            access_control::Method::Specified(Method::GET),
            "/app/workspace",
            Effect::Allow,
            Grantee::All,
        )
        .await
        .unwrap();
    let app = Router::new()
        .fallback(axum::routing::any(|| async {
            // Any stale upstream value must be replaced with the current caller.
            (
                [
                    ("pishoo-viewer-name", "stale.dhttp.net"),
                    ("cache-control", "public, max-age=3600"),
                ],
                "page",
            )
        }))
        .layer(axum::middleware::from_fn_with_state(
            access.clone(),
            authorize,
        ));

    for caller in [None, Some("bob.dhttp.net")] {
        let mut request = anonymous_request();
        *request.method_mut() = Method::HEAD;
        *request.uri_mut() = "/app/workspace/intro".parse().unwrap();
        request
            .headers_mut()
            .insert("pishoo-viewer-name", "forged.dhttp.net".parse().unwrap());
        if let Some(name) = caller {
            request.extensions_mut().remove::<dhttp::HandshakeSummary>();
            request.extensions_mut().insert(
                crate::test_identity::endpoint(name)
                    .local_authority()
                    .unwrap(),
            );
        }
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["pishoo-viewer-name"],
            caller.unwrap_or("")
        );
        assert_eq!(response.headers()["cache-control"], "no-store");
    }
    access
        .set_policy(
            access_control::Method::Specified(Method::GET),
            "/app/workspace",
            Effect::Deny,
            Grantee::All,
        )
        .await
        .unwrap();
    let mut request = anonymous_request();
    *request.method_mut() = Method::HEAD;
    *request.uri_mut() = "/app/workspace/intro".parse().unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(!response.headers().contains_key("pishoo-viewer-name"));
}

#[tokio::test]
async fn frontend_pages_assets_and_head_share_the_root_get_rule_without_opening_backend_apis() {
    use access_control::{Effect, Grantee};
    use tower::ServiceExt;

    let (access, _) = authorization_app(Effect::Deny).await;
    let app = Router::new()
        .fallback(axum::routing::any(
            |request: Request<AxumBody>| async move {
                // Authorization must not rewrite the URI used for static routing.
                request.uri().to_string()
            },
        ))
        .layer(axum::middleware::from_fn_with_state(
            access.clone(),
            authorize,
        ));
    access
        .set_policy(
            access_control::Method::Specified(Method::GET),
            "/app/workspace/assets",
            Effect::Deny,
            Grantee::All,
        )
        .await
        .unwrap();
    access
        .set_policy(
            access_control::Method::Specified(Method::HEAD),
            "/app/workspace",
            Effect::Deny,
            Grantee::All,
        )
        .await
        .unwrap();
    for (effect, expected) in [
        (Effect::Allow, StatusCode::OK),
        (Effect::Deny, StatusCode::FORBIDDEN),
    ] {
        access
            .set_policy(
                access_control::Method::Specified(Method::GET),
                "/app/workspace",
                effect,
                Grantee::All,
            )
            .await
            .unwrap();
        for method in [Method::GET, Method::HEAD] {
            for path in [
                "/app/workspace",
                "/app/workspace/intro?view=1",
                "/app/workspace/assets/app.js",
            ] {
                let mut request = anonymous_request();
                *request.uri_mut() = path.parse().unwrap();
                *request.method_mut() = method.clone();
                let response = app.clone().oneshot(request).await.unwrap();
                assert_eq!(response.status(), expected, "{method} {path}");
                if expected == StatusCode::OK && method == Method::GET {
                    assert_eq!(
                        response.into_body().collect().await.unwrap().to_bytes(),
                        path
                    );
                }
            }
        }
        for path in ["/std/message", "/journal/entries", "/app/workspace-other/"] {
            let mut request = anonymous_request();
            *request.uri_mut() = path.parse().unwrap();
            assert_eq!(
                app.clone().oneshot(request).await.unwrap().status(),
                StatusCode::FORBIDDEN
            );
        }
    }
    access
        .set_policy(
            access_control::Method::Specified(Method::GET),
            "/journal/entries",
            Effect::Allow,
            Grantee::All,
        )
        .await
        .unwrap();
    let mut request = anonymous_request();
    *request.uri_mut() = "/journal/entries".parse().unwrap();
    assert_eq!(app.oneshot(request).await.unwrap().status(), StatusCode::OK);
}

#[tokio::test]
async fn authorization_uses_current_library_allow_and_deny_results() {
    use tower::ServiceExt;
    for (effect, expected) in [
        (access_control::Effect::Allow, StatusCode::NO_CONTENT),
        (access_control::Effect::Deny, StatusCode::FORBIDDEN),
    ] {
        let (access, app) = authorization_app(effect).await;
        for mode in ["waiting", "break"] {
            let mut request = anonymous_request();
            request
                .headers_mut()
                .insert("if-reviewed", mode.parse().unwrap());
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), expected);
        }
        assert_eq!(access.pending_persistent_reviews(0, 10).await.unwrap().0, 0);
    }
}

#[tokio::test]
async fn authorization_requires_a_trusted_summary_even_for_anonymous_allow() {
    use tower::ServiceExt;
    let (_, app) = authorization_app(access_control::Effect::Allow).await;
    let mut request = anonymous_request();
    request.extensions_mut().remove::<dhttp::HandshakeSummary>();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn anonymous_reviews_are_denied_without_creating_a_record() {
    use tower::ServiceExt;
    let (access, app) = authorization_app(access_control::Effect::Review).await;
    assert_eq!(
        app.oneshot(anonymous_request()).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(access.pending_persistent_reviews(0, 10).await.unwrap().0, 0);
}

#[tokio::test]
async fn persistent_reviews_are_scoped_to_the_subject_and_write_durable_acl() {
    use access_control::{AuthResult, Effect, Grantee, Headers, PendingReviewResponse};
    use tower::ServiceExt;
    let (access, _) = authorization_app(Effect::Deny).await;
    access
        .set_policy(
            access_control::Method::Unspecified,
            "/protected",
            Effect::Review,
            Grantee::Named,
        )
        .await
        .unwrap();
    let visitor = Visitor::new("alice.dhttp.net", SubjectId::new(b"alice-owner").unwrap());
    let headers = Headers {
        method: Method::GET,
        path: "/protected?x=1".into(),
        fields: http::HeaderMap::new(),
    };
    let AuthResult::Reviewing(review) = access
        .auth(
            headers.clone(),
            Some(visitor.name()),
            Some(visitor.subject_id()),
        )
        .await
        .unwrap()
    else {
        panic!("expected a persistent review")
    };
    let id = review.id();
    let pending = serde_json::to_value(PendingReviewResponse::from(review)).unwrap();
    let path = pending["status_url"].as_str().unwrap();
    let app = access_router(access.clone());
    for (subject, expected) in [
        (visitor.subject_id().clone(), StatusCode::OK),
        (
            SubjectId::new(b"different-owner").unwrap(),
            StatusCode::NOT_FOUND,
        ),
    ] {
        let mut request = Request::builder()
            .uri(path)
            .body(AxumBody::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(Visitor::new(visitor.name(), subject));
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            expected
        );
    }
    access.decide_review(id, Action::Allow, true).await.unwrap();
    assert!(matches!(
        access
            .auth(
                headers.clone(),
                Some(visitor.name()),
                Some(visitor.subject_id())
            )
            .await
            .unwrap(),
        AuthResult::Allowed
    ));
    assert!(matches!(
        access
            .auth(headers, Some(visitor.name()), Some(visitor.subject_id()))
            .await
            .unwrap(),
        AuthResult::Allowed
    ));
}

#[tokio::test]
async fn contact_approval_is_local_and_the_applicant_reads_its_relationship() {
    use tower::ServiceExt;
    let (access, _) = authorization_app(access_control::Effect::Deny).await;
    let app = access_router(access.clone());
    let visitor = Visitor::new("peer.dhttp.net", SubjectId::new(b"peer-owner").unwrap());
    let application_id = "a".repeat(64);
    let mut application = Request::builder()
        .method(Method::POST)
        .uri("/std/contact")
        .header(header::CONTENT_TYPE, "application/json")
        .body(AxumBody::from(
            serde_json::json!({
                "from":"peer.dhttp.net", "to":"owner.dhttp.net",
                "application_id": application_id, "description": "Chat",
                "requested_access": {"/std/message": ["POST"]}, "offers": {},
            })
            .to_string(),
        ))
        .unwrap();
    application.extensions_mut().insert(visitor.clone());
    assert_eq!(
        app.clone().oneshot(application).await.unwrap().status(),
        StatusCode::CREATED
    );
    let mut approval = Request::builder()
        .method(Method::PATCH)
        .uri("/std/contact?name=peer.dhttp.net")
        .header(header::CONTENT_TYPE, "application/json")
        .body(AxumBody::from(r#"{"status":"active"}"#))
        .unwrap();
    approval.extensions_mut().insert(Visitor::new(
        "owner.dhttp.net",
        SubjectId::new([1]).unwrap(),
    ));
    assert_eq!(
        app.clone().oneshot(approval).await.unwrap().status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        access
            .find_contact_by_name(visitor.name())
            .await
            .unwrap()
            .status,
        2
    );
    let mut status = Request::builder()
        .uri("/std/contact?name=peer.dhttp.net")
        .body(AxumBody::empty())
        .unwrap();
    status.extensions_mut().insert(visitor);
    let response = app.oneshot(status).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap()["status"],
        "active"
    );
}

#[tokio::test]
async fn status_routes_reject_a_forged_visitor_on_an_anonymous_connection() {
    use tower::ServiceExt;
    let (access, _) = authorization_app(access_control::Effect::Allow).await;
    let app = access_router(access.clone())
        .layer(axum::middleware::from_fn_with_state(access, authorize));
    for (path, expected) in [
        ("/std/acl/status?id=1", StatusCode::FORBIDDEN),
        ("/std/contact/status", StatusCode::FORBIDDEN),
    ] {
        let mut request = anonymous_request();
        *request.uri_mut() = path.parse().unwrap();
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            expected
        );
    }
}

fn reviewed_request(mode: &str) -> Request<AxumBody> {
    let mut request = anonymous_request();
    let name = "alice.dhttp.net";
    let mut params = rcgen::CertificateParams::new(vec![name.into()]).unwrap();
    let ski = format!("0:0:{}", "a".repeat(64));
    let mut der = vec![4, ski.len() as u8];
    der.extend_from_slice(ski.as_bytes());
    params
        .custom_extensions
        .push(rcgen::CustomExtension::from_oid_content(
            &[2, 5, 29, 14],
            der,
        ));
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = params.self_signed(&key).unwrap();
    let local = dhttp::LocalAuthority::new(
        &qtls::default_provider(),
        Arc::from(name),
        vec![cert.der().clone()],
        qtls::PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
        vec![1],
    )
    .unwrap();
    request.extensions_mut().remove::<dhttp::HandshakeSummary>();
    request.extensions_mut().insert(local);
    request
        .headers_mut()
        .insert("if-reviewed", mode.parse().unwrap());
    request
}

#[tokio::test]
async fn business_requests_trigger_break_and_waiting_reviews_without_a_permission_preflight() {
    use tower::ServiceExt;
    let (access, _) = authorization_app(access_control::Effect::Deny).await;
    access
        .set_policy(
            access_control::Method::Unspecified,
            "/protected",
            access_control::Effect::Review,
            access_control::Grantee::Named,
        )
        .await
        .unwrap();
    let app = Router::new()
        .route(
            "/protected",
            axum::routing::get(|| async { StatusCode::NO_CONTENT }),
        )
        .layer(axum::middleware::from_fn_with_state(
            access.clone(),
            authorize,
        ));
    let response = app
        .clone()
        .oneshot(reviewed_request("break"))
        .await
        .unwrap();
    assert_eq!(response.status(), 202);
    assert_eq!(response.headers()["retry-after"], "3");
    let pending: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(pending["status"], "pending_review");
    assert!(pending.get("error").is_none());
    assert_eq!(access.pending_persistent_reviews(0, 10).await.unwrap().0, 1);
    let management = access_router(access.clone());
    for action in ["allow", "deny", "cancel"] {
        access
            .remove_policy(
                access_control::Method::Specified(Method::GET),
                "/protected",
                access_control::Grantee::One("alice.dhttp.net".into()),
            )
            .await
            .unwrap();
        let task = tokio::spawn(app.clone().oneshot(reviewed_request("waiting")));
        let live = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let response = management
                    .clone()
                    .oneshot(
                        Request::builder()
                            .uri("/std/acl/review/list?kind=live")
                            .body(AxumBody::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                let body: serde_json::Value = serde_json::from_slice(
                    &response.into_body().collect().await.unwrap().to_bytes(),
                )
                .unwrap();
                if body["total"] == 1 {
                    break body["items"][0].clone();
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(live["kind"], "live");
        assert!(live["expired_after"].is_null());
        if action == "cancel" {
            task.abort();
            let _ = task.await;
        } else {
            let response = management
                .clone()
                .oneshot(
                    Request::builder()
                        .method("PATCH")
                        .uri("/std/acl/review")
                        .header("content-type", "application/json")
                        .body(AxumBody::from(
                            serde_json::json!({"kind":"live","id":live["id"],"action":action,"remember":true})
                                .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), 204);
            assert_eq!(
                task.await.unwrap().unwrap().status(),
                if action == "allow" { 204 } else { 403 }
            );
            for mode in ["break", "waiting"] {
                assert_eq!(
                    app.clone()
                        .oneshot(reviewed_request(mode))
                        .await
                        .unwrap()
                        .status(),
                    if action == "allow" { 204 } else { 403 }
                );
            }
        }
        let response = management
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/std/acl/review/list?kind=live")
                    .body(AxumBody::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body["total"], 0);
    }
    assert_eq!(access.pending_persistent_reviews(0, 10).await.unwrap().0, 0);
    assert_eq!(
        app.oneshot(reviewed_request("invalid"))
            .await
            .unwrap()
            .status(),
        400
    );
}

#[tokio::test]
async fn waiting_timeout_cleans_up_without_granting_acl() {
    use sea_orm::ConnectionTrait;
    use tower::ServiceExt;
    let (access, _) = authorization_app(access_control::Effect::Deny).await;
    access
        .set_policy(
            access_control::Method::Unspecified,
            "/protected",
            access_control::Effect::Review,
            access_control::Grantee::Named,
        )
        .await
        .unwrap();
    let app = Router::new()
        .route(
            "/protected",
            axum::routing::get(|| async { StatusCode::NO_CONTENT }),
        )
        .layer(axum::middleware::from_fn_with_state(
            access.clone(),
            authorize,
        ));
    tokio::time::pause();
    let task = tokio::spawn(app.oneshot(reviewed_request("waiting")));
    let management = access_router(access.clone());
    loop {
        let response = management
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/std/acl/review/list?kind=live")
                    .body(AxumBody::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        if body["total"] == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    tokio::time::advance(std::time::Duration::from_secs(301)).await;
    let response = task.await.unwrap().unwrap();
    assert_eq!(response.status(), 504);
    let body: serde_json::Value = serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["error"], "review_timeout");
    let response = management
        .oneshot(
            Request::builder()
                .uri("/std/acl/review/list?kind=live")
                .body(AxumBody::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["total"], 0);
    tokio::time::resume();
    assert_eq!(access.pending_persistent_reviews(0, 10).await.unwrap().0, 0);
    let row = access
        .database()
        .query_one_raw(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            "SELECT count(*) AS n FROM access_rules WHERE grantee='alice.dhttp.net'",
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.try_get::<i64>("", "n").unwrap(), 0);
}
