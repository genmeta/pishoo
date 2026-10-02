use super::*;

#[tokio::test]
async fn authorization_uses_current_library_allow_and_deny_results() {
    use tower::ServiceExt;
    for (effect, expected) in [
        (access_control::Effect::Allow, StatusCode::NO_CONTENT),
        (access_control::Effect::Deny, StatusCode::FORBIDDEN),
    ] {
        let (access, app) = authorization_app(effect).await;
        let response = app.oneshot(anonymous_request()).await.unwrap();
        assert_eq!(response.status(), expected);
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
async fn persistent_reviews_are_scoped_to_the_subject_and_consumed_on_retry() {
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
    access
        .decide_review(
            id,
            Action::Allow,
            chrono::Utc::now() + chrono::Duration::minutes(5),
        )
        .await
        .unwrap();
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
        AuthResult::Reviewing(_)
    ));
}

#[tokio::test]
async fn contact_approval_is_local_and_the_applicant_polls_its_application() {
    use tower::ServiceExt;
    let (access, _) = authorization_app(access_control::Effect::Deny).await;
    let app = access_router(access.clone());
    let visitor = Visitor::new("peer.dhttp.net", SubjectId::new(b"peer-owner").unwrap());
    let application_id = "a".repeat(64);
    let mut application = Request::builder()
        .method(Method::POST)
        .uri("/contact")
        .header(header::CONTENT_TYPE, "application/json")
        .body(AxumBody::from(
            serde_json::json!({
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
        .uri("/contact/peer.dhttp.net")
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
        .uri(format!("/contact/self?application_id={application_id}"))
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
        ("/acl/review/1/status", StatusCode::FORBIDDEN),
        ("/contact/self", StatusCode::BAD_REQUEST),
    ] {
        let mut request = anonymous_request();
        *request.uri_mut() = path.parse().unwrap();
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            expected
        );
    }
}
