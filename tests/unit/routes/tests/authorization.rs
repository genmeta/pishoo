use super::*;

#[tokio::test]
async fn static_cache_validation_and_ranges_never_skip_current_authorization() {
    use access_control::{Effect, Grantee};
    use tower::ServiceExt;

    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("index.html"), b"0123456789").unwrap();
    let directory = Arc::new(std::fs::File::open(root.path()).unwrap());
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
        .fallback(axum::routing::any(move |request: Request<AxumBody>| {
            let directory = directory.clone();
            async move {
                tokio::task::spawn_blocking(move || {
                    crate::sandbox::app_response(&directory, "/", &request).unwrap()
                })
                .await
                .unwrap()
            }
        }))
        .layer(axum::middleware::from_fn_with_state(
            access.clone(),
            authorize,
        ));
    let mut initial = anonymous_request();
    *initial.uri_mut() = "/app/workspace/".parse().unwrap();
    let response = app.clone().oneshot(initial).await.unwrap();
    let etag = response.headers()[header::ETAG]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "private, no-cache"
    );

    for (caller, expected) in [(None, ""), (Some("bob.dhttp.net"), "bob.dhttp.net")] {
        let mut request = anonymous_request();
        *request.uri_mut() = "/app/workspace/".parse().unwrap();
        *request.method_mut() = Method::HEAD;
        request
            .headers_mut()
            .insert(header::IF_NONE_MATCH, etag.parse().unwrap());
        if let Some(caller) = caller {
            request.extensions_mut().remove::<dhttp::HandshakeSummary>();
            request.extensions_mut().insert(
                crate::test_support::identity::endpoint(caller)
                    .local_authority()
                    .unwrap(),
            );
        }
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(response.headers()["pishoo-viewer-name"], expected);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(response.headers()["x-frame-options"], "DENY");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "10");
        assert!(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .is_empty()
        );
    }
    for effect in [Effect::Allow, Effect::Deny] {
        access
            .set_policy(
                access_control::Method::Specified(Method::GET),
                "/app/workspace",
                effect,
                Grantee::All,
            )
            .await
            .unwrap();
        for conditional in [true, false] {
            let mut request = anonymous_request();
            *request.uri_mut() = "/app/workspace/".parse().unwrap();
            request
                .headers_mut()
                .insert(header::RANGE, "bytes=3-5".parse().unwrap());
            if conditional {
                request
                    .headers_mut()
                    .insert(header::IF_NONE_MATCH, etag.parse().unwrap());
            }
            let response = app.clone().oneshot(request).await.unwrap();
            let expected = if effect == Effect::Deny {
                StatusCode::FORBIDDEN
            } else if conditional {
                StatusCode::NOT_MODIFIED
            } else {
                StatusCode::PARTIAL_CONTENT
            };
            assert_eq!(response.status(), expected);
            if effect == Effect::Deny {
                assert!(!response.headers().contains_key(header::ETAG));
                assert!(!response.headers().contains_key(header::CONTENT_RANGE));
                assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            } else if !conditional {
                assert_eq!(
                    response.into_body().collect().await.unwrap().to_bytes(),
                    "345"
                );
            }
        }
    }
}

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
                crate::test_support::identity::endpoint(name)
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
        for path in [
            "/std/comm/message",
            "/journal/entries",
            "/app/workspace-other/",
        ] {
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
                "requested_access": {"/std/comm/message": ["POST"]}, "offers": {},
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
    let body: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
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

#[tokio::test]
async fn hierarchical_app_pages_and_head_use_the_complete_registered_root_permission() {
    use tower::ServiceExt;
    let (access, _) = authorization_app(access_control::Effect::Deny).await;
    access
        .set_policy(
            access_control::Method::Specified(Method::GET),
            "/app/live/dashboard",
            access_control::Effect::Allow,
            access_control::Grantee::All,
        )
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut sandbox = crate::sandbox::Sandbox::new(Arc::new(crate::WasmRuntime::new().unwrap()));
    sandbox.apps.insert(
        "alice.dhttp.net/live/dashboard".into(),
        (
            "1.0.0".into(),
            Arc::new(std::fs::File::open(root.path()).unwrap()),
        ),
    );
    let app = Router::new()
        .fallback(axum::routing::any(|| async { StatusCode::NO_CONTENT }))
        .layer(axum::middleware::from_fn_with_state(access, authorize))
        .layer(axum::Extension(Arc::new(std::sync::RwLock::new(sandbox))));
    for method in [Method::GET, Method::HEAD] {
        for path in [
            "/app/live/dashboard",
            "/app/live/dashboard/assets/app.js",
            "/app/live/dashboard/settings/account",
        ] {
            let mut request = anonymous_request();
            *request.uri_mut() = path.parse().unwrap();
            *request.method_mut() = method.clone();
            assert_eq!(
                app.clone().oneshot(request).await.unwrap().status(),
                StatusCode::NO_CONTENT,
                "{method} {path}"
            );
        }
    }
    let mut request = anonymous_request();
    *request.uri_mut() = "/app/live/other/".parse().unwrap();
    assert_eq!(
        app.oneshot(request).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
}

#[test]
fn communication_resources_share_canonical_path_spelling() {
    use crate::routes::resource::canonical;
    for (raw, expected) in [
        (
            "/std/comm/message/ZERO.ai.dhttp.net/",
            "/std/comm/message/zero.ai",
        ),
        (
            "/std/comm/message/histor%79.dhttp.net",
            "/std/comm/message/history.dhttp.net",
        ),
        (
            "/std/comm/group/usopp.li.dhttp.net~%23dev/member/%7Aero.ai.dhttp.net/",
            "/std/comm/group/usopp.li~%23dev/member/zero.ai",
        ),
        ("/std/contact/list.dhttp.net", "/std/contact/list.dhttp.net"),
        ("/std/contact/li%73t", "/std/contact/list"),
    ] {
        assert_eq!(canonical(raw).unwrap(), expected);
    }
    for tail in [
        "usopp.li%2Fdev",
        "usopp.li%252Fdev",
        "%2e%2e",
        "zero.ai//file",
        "usopp.li~%2523dev",
        "zero%5cai",
        "%Z1",
        "%00",
    ] {
        assert!(
            canonical(&format!("/std/comm/message/{tail}")).is_err(),
            "{tail}"
        );
    }
}

#[tokio::test]
async fn legacy_and_named_private_messages_use_one_effective_policy() {
    use access_control::{Effect, Grantee};
    use tower::ServiceExt;
    let (access, _) = authorization_app(Effect::Deny).await;
    let peer = crate::test_support::identity::endpoint("zero.ai.dhttp.net");
    let endpoint = crate::test_support::identity::endpoint("usopp.li.dhttp.net");
    let rule = access_control::Method::Specified(Method::POST);
    access
        .set_policy(
            rule.clone(),
            "/std/comm/message/zero.ai",
            Effect::Allow,
            Grantee::Named,
        )
        .await
        .unwrap();
    let app = Router::new()
        .fallback(axum::routing::any(|| async { StatusCode::NO_CONTENT }))
        .layer(axum::middleware::from_fn_with_state(
            access.clone(),
            authorize,
        ))
        .layer(axum::Extension(endpoint))
        .layer(axum::middleware::from_fn(normalize_method));
    let send = |path: &str| {
        Request::post(path)
            .extension(peer.local_authority().unwrap().clone())
            .header("content-type", "application/json")
            .body(AxumBody::from(
                r#"{"type":"text","to":"usopp.li","content":"hi","client_message_id":"1"}"#,
            ))
            .unwrap()
    };
    for (effect, expected) in [
        (Effect::Allow, 204),
        (Effect::Deny, 403),
        (Effect::Review, 202),
    ] {
        access
            .set_policy(
                rule.clone(),
                "/std/comm/message/zero.ai",
                effect,
                Grantee::Named,
            )
            .await
            .unwrap();
        let legacy = app
            .clone()
            .oneshot(send("/std/comm/message"))
            .await
            .unwrap();
        let resource = app
            .clone()
            .oneshot(send("/std/comm/message/zero.ai.dhttp.net/"))
            .await
            .unwrap();
        assert_eq!(legacy.status(), expected);
        assert_eq!(resource.status(), expected);
        if expected == 202 {
            assert_eq!(
                legacy.headers()[header::LOCATION],
                resource.headers()[header::LOCATION]
            );
        }
    }
}

#[tokio::test]
async fn legacy_private_file_grants_and_group_intake_do_not_override_specific_decisions() {
    use access_control::{Effect, Grantee};
    use sea_orm::ConnectionTrait;

    use crate::routes::resource::compatibility_operation;
    let (access, _) = authorization_app(Effect::Deny).await;
    let actor = "zero.ai.dhttp.net";
    let local = "usopp.li.dhttp.net";
    let visitor = Visitor::new(actor, SubjectId::new(b"aa").unwrap());
    access.database().execute_unprepared("INSERT INTO contacts(name,subject_id,status,created_at,updated_at,expired_after) VALUES('zero.ai.dhttp.net',X'6161',2,1,1,2000000000)").await.unwrap();
    let exact = Grantee::One(actor.into());
    let post = access_control::Method::Specified(Method::POST);
    access
        .set_policy(
            post.clone(),
            "/std/comm/message",
            Effect::Allow,
            exact.clone(),
        )
        .await
        .unwrap();
    let different = Visitor::new(actor, SubjectId::new(b"other").unwrap());
    let file_path = "/std/comm/message/zero.ai/file/001";
    assert_eq!(
        compatibility_operation(
            &access,
            Method::HEAD,
            file_path.into(),
            Some(&different),
            Some(local)
        )
        .await
        .unwrap(),
        (Method::HEAD, file_path.into())
    );
    let file = "/std/comm/message/zero.ai/file/a1";
    let resolve = |method: Method, path: &str| {
        compatibility_operation(
            &access,
            method,
            path.to_owned(),
            Some(&visitor),
            Some(local),
        )
    };
    assert_eq!(
        resolve(Method::GET, file).await.unwrap(),
        (Method::POST, file.into())
    );
    access
        .set_policy(
            access_control::Method::Specified(Method::GET),
            file,
            Effect::Deny,
            exact.clone(),
        )
        .await
        .unwrap();
    assert_eq!(
        resolve(Method::GET, file).await.unwrap(),
        (Method::GET, file.into())
    );
    assert_eq!(
        resolve(Method::HEAD, file).await.unwrap(),
        (Method::POST, file.into())
    );
    access
        .remove_policy(post.clone(), "/std/comm/message", exact.clone())
        .await
        .unwrap();
    access
        .set_policy(
            post.clone(),
            "/std/comm/message/zero.ai",
            Effect::Allow,
            exact.clone(),
        )
        .await
        .unwrap();
    assert_eq!(
        resolve(Method::HEAD, file).await.unwrap(),
        (Method::HEAD, file.into())
    );
    let group_file = "/std/comm/message/usopp.li~%23dev/file/abc";
    let conversation = "/std/comm/message/usopp.li~%23dev";
    access
        .set_policy(post.clone(), conversation, Effect::Allow, exact.clone())
        .await
        .unwrap();
    assert_eq!(
        resolve(Method::PATCH, group_file).await.unwrap(),
        (Method::PATCH, group_file.into())
    );
    access
        .set_policy(post, "/std/comm/group", Effect::Allow, exact.clone())
        .await
        .unwrap();
    let member = "/std/comm/group/usopp.li~%23dev/member/zero.ai";
    assert_eq!(resolve(Method::PUT, member).await.unwrap().0, Method::POST);
    for verb in [Method::PATCH, Method::DELETE] {
        let self_intake = resolve(verb.clone(), member).await.unwrap();
        assert_eq!(self_intake.0, Method::POST);
        let other = "/std/comm/group/usopp.li~%23dev/member/someone";
        assert_eq!(
            resolve(verb.clone(), other).await.unwrap(),
            (verb.clone(), other.into())
        );
        access
            .set_policy(
                access_control::Method::Specified(verb.clone()),
                member,
                Effect::Deny,
                exact.clone(),
            )
            .await
            .unwrap();
        assert_eq!(
            resolve(verb.clone(), member).await.unwrap(),
            (verb, member.into())
        );
    }
    let other = "/std/comm/group/usopp.li~%23dev/member/another";
    assert_eq!(
        resolve(Method::PUT, other).await.unwrap(),
        (Method::PUT, other.into())
    );
    access
        .set_policy(
            access_control::Method::Specified(Method::PUT),
            member,
            Effect::Review,
            exact,
        )
        .await
        .unwrap();
    assert_eq!(
        resolve(Method::PUT, member).await.unwrap(),
        (Method::PUT, member.into())
    );
}

#[tokio::test]
async fn revoking_event_access_closes_an_open_workspace_stream_before_the_next_frame() {
    check_stream_revocation("/std/comm/message/history", "/std/comm/message/history").await;
}

#[tokio::test]
async fn revoking_forward_access_closes_the_remote_event_stream_before_the_next_frame() {
    check_stream_revocation(
        "/std/forward/host.dhttp.net/std/comm/message/history",
        "/std/forward",
    )
    .await;
}

async fn check_stream_revocation(path: &'static str, grant: &str) {
    use access_control::{Effect, Grantee};
    use tower::ServiceExt;
    let (access, _) = authorization_app(Effect::Deny).await;
    let method = access_control::Method::Unspecified;
    access
        .set_policy(method.clone(), grant, Effect::Allow, Grantee::Named)
        .await
        .unwrap();
    let release = Arc::new(tokio::sync::Notify::new());
    let resume = release.clone();
    let handler = axum::routing::get(move || {
        let resume = resume.clone();
        async move {
            let frames = async_stream::stream! {
                yield Ok::<_,std::convert::Infallible>(bytes::Bytes::from_static(b"data: first\n\n"));
                resume.notified().await;
                yield Ok(bytes::Bytes::from_static(b"data: private\n\n"));
            };
            http::Response::builder()
                .header(header::CONTENT_TYPE, "text/event-stream")
                .body(AxumBody::from_stream(frames))
                .unwrap()
        }
    });
    let app = Router::new()
        .route(path, handler)
        .layer(axum::middleware::from_fn_with_state(
            access.clone(),
            authorize,
        ));
    let peer = crate::test_support::identity::endpoint("zero.ai.dhttp.net");
    let request = || {
        Request::get(path)
            .extension(peer.local_authority().unwrap())
            .body(AxumBody::empty())
            .unwrap()
    };
    let response = app.clone().oneshot(request()).await.unwrap();
    assert_eq!(response.status(), 200);
    let mut body = response.into_body();
    assert_eq!(
        body.frame().await.unwrap().unwrap().into_data().unwrap(),
        bytes::Bytes::from_static(b"data: first\n\n")
    );
    access
        .remove_policy(method, grant, Grantee::Named)
        .await
        .unwrap();
    release.notify_one();
    assert!(body.frame().await.is_none());
    assert_eq!(app.oneshot(request()).await.unwrap().status(), 403);
}
