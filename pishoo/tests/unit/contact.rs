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
    grant_offered(
        &access,
        &endpoint,
        target,
        &subject,
        &[],
        &selected,
        now() + 7 * 86400,
    )
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
        grant_offered(
            &access,
            &endpoint,
            target,
            &"c".repeat(64),
            &[],
            &selected,
            now() + 7 * 86400
        )
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
            "/std/contact?name=bob.dhttp.net",
            Some(&json!({"status":status})),
        )
        .await
        .unwrap();
    }
    assert_eq!(
        grant_offered(
            &access,
            &endpoint,
            target,
            &subject,
            &[],
            &selected,
            now() + 7 * 86400
        )
        .await
        .unwrap_err()
        .0,
        409
    );
}

#[tokio::test]
async fn contact_routes_follow_acl_and_accept_an_authorized_manager() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.join("db")).unwrap();
    let endpoint = crate::test_identity::endpoint("alice.dhttp.net");
    let manager = crate::test_identity::endpoint("bob.dhttp.net");
    let access = Arc::new(
        AccessService::load_from_db(
            "sqlite::memory:",
            endpoint.name(),
            owner(&endpoint).unwrap().subject_id(),
        )
        .await
        .unwrap(),
    );
    let router = contact_router(profile.clone(), endpoint.clone(), access.clone()).layer(
        axum::middleware::from_fn_with_state(access.clone(), crate::routes::authorize),
    );
    let request = || {
        let mut request = Request::builder()
            .uri("/std/contact/requests")
            .body(Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(manager.local_authority().unwrap());
        request
    };
    assert_eq!(
        router.clone().oneshot(request()).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    assert!(!profile.join("db/contact.db").exists());
    access
        .set_policy(
            access_control::Method::Specified(Method::GET),
            "/std/contact/requests",
            access_control::Effect::Allow,
            access_control::Grantee::Named,
        )
        .await
        .unwrap();
    assert_eq!(
        router.clone().oneshot(request()).await.unwrap().status(),
        StatusCode::OK
    );
    assert!(profile.join("db/contact.db").exists());
    let mut cross_site = Request::builder()
        .method(Method::POST)
        .uri("/std/contact")
        .header("sec-fetch-site", "cross-site")
        .body(Body::empty())
        .unwrap();
    cross_site
        .extensions_mut()
        .insert(endpoint.local_authority().unwrap());
    assert_eq!(
        router.oneshot(cross_site).await.unwrap().status(),
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
        db.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name='saved_contacts'",
            [],
            |row| row.get::<_, i64>(0)
        )
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

#[tokio::test]
async fn contact_submission_without_capabilities_queues_without_contacting_the_peer() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.join("db")).unwrap();
    let endpoint = crate::test_identity::endpoint("alice.dhttp.net");
    let access = Arc::new(
        AccessService::load_from_db(
            "sqlite::memory:",
            endpoint.name(),
            owner(&endpoint).unwrap().subject_id(),
        )
        .await
        .unwrap(),
    );
    let router = contact_router(profile.clone(), endpoint.clone(), access.clone()).layer(
        axum::middleware::from_fn_with_state(access.clone(), crate::routes::authorize),
    );
    let mut request = Request::builder().method(Method::POST).uri("/std/contact")
        .header("content-type", "application/json")
        .body(Body::from(json!({"target_name":"offline.dhttp.net","description":"Hello","requested_capabilities":[],"offered_capabilities":[]}).to_string())).unwrap();
    request
        .extensions_mut()
        .insert(endpoint.local_authority().unwrap());
    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let body: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["status"], "queued");
    assert_eq!(body["target_name"], "offline.dhttp.net");
    assert!(
        access
            .find_contact_by_name("offline.dhttp.net")
            .await
            .is_err()
    );
    assert!(
        !profile.config_db_path().exists(),
        "no capability catalog is needed"
    );
}
