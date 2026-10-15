use super::*;

#[tokio::test]
async fn offered_capabilities_create_a_pinned_contact_before_granting_selected_operations() {
    let endpoint = crate::test_identity::endpoint("alice.dhttp.net");
    let owner = owner(&endpoint).unwrap();
    let access = Arc::new(
        AccessService::load_from_db("sqlite::memory:", endpoint.name(), owner.subject_id())
            .await
            .unwrap(),
    );
    let target = "bob.dhttp.net";
    let subject = "b".repeat(64);
    let selected = vec![
        json!({"id":"dev.dhttp.net/message#message","endpoints":[{"method":"POST","path":"/std/message"}]}),
    ];
    assert!(contact(&access, &endpoint, target).await.is_err());
    grant_offered(&access, &endpoint, target, &subject, &selected)
        .await
        .unwrap();
    let pending = contact(&access, &endpoint, target).await.unwrap();
    assert_eq!(pending["status"], "pending");
    assert_eq!(pending["subject_id"], subject);
    let rules = local_api(&access, &endpoint, Method::GET, "/std/acl/allow", None)
        .await
        .unwrap();
    assert_eq!(
        rules[target],
        json!({"/std/message":{"allow":["POST"],"review":[],"deny":[]}})
    );

    assert_eq!(
        grant_offered(&access, &endpoint, target, &"c".repeat(64), &selected)
            .await
            .unwrap_err()
            .0,
        409
    );
    for status in ["active", "blocked"] {
        local_api(
            &access,
            &endpoint,
            Method::PATCH,
            "/std/contact/bob.dhttp.net",
            Some(&json!({"status":status})),
        )
        .await
        .unwrap();
    }
    assert_eq!(
        grant_offered(&access, &endpoint, target, &subject, &selected)
            .await
            .unwrap_err()
            .0,
        409
    );
}

#[tokio::test]
async fn local_contact_routes_require_the_real_owner_before_opening_storage() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.join("db")).unwrap();
    let endpoint = crate::test_identity::endpoint("alice.dhttp.net");
    let access = Arc::new(AccessService::new(
        sea_orm::DatabaseConnection::default(),
        endpoint.name(),
    ));
    let router = contact_router(profile.clone(), endpoint.clone(), access);
    for visitor in [
        None,
        Some(access_control::Visitor::new(
            "bob.dhttp.net",
            access_control::SubjectId::new("a".repeat(64).as_bytes()).unwrap(),
        )),
        Some(access_control::Visitor::new(
            "alice.dhttp.net",
            access_control::SubjectId::new("b".repeat(64).as_bytes()).unwrap(),
        )),
    ] {
        let mut request = Request::builder()
            .uri("/std/contact/local/requests")
            .body(Body::empty())
            .unwrap();
        if let Some(visitor) = visitor {
            request.extensions_mut().insert(visitor);
        }
        assert_eq!(
            router.clone().oneshot(request).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        assert!(!profile.join("db/contact.db").exists());
    }
    let mut request = Request::builder()
        .uri("/std/contact/local/requests")
        .body(Body::empty())
        .unwrap();
    request.extensions_mut().insert(owner(&endpoint).unwrap());
    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap()["total"], 0);
    let mut request = Request::builder()
        .method(Method::POST)
        .uri("/std/contact/local/requests")
        .header("sec-fetch-site", "cross-site")
        .body(Body::empty())
        .unwrap();
    request.extensions_mut().insert(owner(&endpoint).unwrap());
    assert_eq!(
        router.oneshot(request).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
}

#[test]
fn expired_claims_cannot_resurrect_requests_and_database_reopens_without_private_permissions() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.join("db")).unwrap();
    let db = database(&profile).unwrap();
    db.execute("INSERT INTO outbound_contact_requests(target_name,status,expired_after,delivery_deadline,next_attempt_at,lease_until,created_at,updated_at) VALUES('bob.dhttp.net','queued',?1,?1,0,?2,0,0)", params![now()-1,now()+120]).unwrap();
    let old_lease = now() + 120;
    expire_requests(&db).unwrap();
    assert_eq!(
        db.execute(
            "UPDATE outbound_contact_requests SET status='active' WHERE id=1 AND lease_until=?1",
            [old_lease]
        )
        .unwrap(),
        0
    );
    drop(db);
    let db = database(&profile).unwrap();
    assert_eq!(outbound_request(&db, 1).unwrap()["status"], "failed");
    assert_eq!(db.query_row("SELECT count(*) FROM sqlite_master WHERE name IN ('profile_preferences','capability_decisions','capability_decision_events','chat_messages')",[],|row|row.get::<_,i64>(0)).unwrap(),0);
    assert_eq!(
        db.query_row("SELECT count(*) FROM saved_contacts", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn contact_storage_does_not_adopt_unknown_or_incomplete_databases() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.join("db")).unwrap();
    let db = Connection::open(profile.join("db/contact.db")).unwrap();
    db.execute_batch("CREATE TABLE unrelated(id INTEGER)")
        .unwrap();
    assert!(database(&profile).is_err());
    db.execute_batch("DROP TABLE unrelated; PRAGMA user_version=1")
        .unwrap();
    assert!(database(&profile).is_err());
}
