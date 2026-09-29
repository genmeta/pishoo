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
        assert_eq!(access.pending_live_reviews(0, 10).0, 0);
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
async fn reviews_wait_in_the_request_then_apply_allow_or_deny() {
    use tower::ServiceExt;
    for (action, expected) in [
        (Action::Allow, StatusCode::NO_CONTENT),
        (Action::Deny, StatusCode::FORBIDDEN),
    ] {
        let (access, app) = authorization_app(access_control::Effect::Review).await;
        let response = tokio::spawn(app.oneshot(anonymous_request()));
        let id = pending_review(&access).await;
        assert!(
            !response.is_finished(),
            "a pending review must not return 202 or a final response"
        );
        access
            .decide_review(access_control::ReviewTarget::Live(id), action, None)
            .await
            .unwrap();
        assert_eq!(response.await.unwrap().unwrap().status(), expected);
        assert_eq!(access.pending_live_reviews(0, 10).0, 0);
        assert!(
            !access.reviews().del(id),
            "request cleanup must remove its registry entry"
        );
    }
}

#[tokio::test]
async fn abandoning_a_review_removes_its_live_registration() {
    use tower::ServiceExt;
    let (access, app) = authorization_app(access_control::Effect::Review).await;
    let response = tokio::spawn(app.oneshot(anonymous_request()));
    let id = pending_review(&access).await;
    response.abort();
    assert!(response.await.unwrap_err().is_cancelled());
    assert_eq!(access.pending_live_reviews(0, 10).0, 0);
    assert!(!access.reviews().del(id));
    assert!(
        access
            .decide_review(access_control::ReviewTarget::Live(id), Action::Allow, None)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn contact_approval_uses_notifier_and_keeps_syncing_after_failure() {
    use access_control::NewContact;
    use tower::ServiceExt;

    let owner = "owner.dhttp.net";
    let contact = "peer.dhttp.net";
    let owner_subject = SubjectId::new([1]).unwrap();
    let access = Arc::new(
        AccessService::load_from_db("sqlite::memory:", owner, &owner_subject)
            .await
            .unwrap(),
    );
    access
        .create_contact(NewContact {
            name: contact.into(),
            subject_id: SubjectId::new([2]).unwrap(),
            class: String::new(),
            description: String::new(),
            requested_access: Default::default(),
            offers: Default::default(),
            created_at: i64::MAX - 1,
            updated_at: i64::MAX - 1,
            expired_after: i64::MAX,
        })
        .await
        .unwrap();
    let endpoint = dhttp::Endpoint::load(owner).await.unwrap();
    let app = access_router(access.clone(), endpoint, owner, owner);
    let mut request = Request::builder()
        .method(Method::PATCH)
        .uri(format!("/contact/{contact}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(AxumBody::from(r#"{"status":"syncing"}"#))
        .unwrap();
    request
        .extensions_mut()
        .insert(Visitor::new(owner, owner_subject));

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        access.find_contact_by_name(contact).await.unwrap().status,
        1
    );
}
