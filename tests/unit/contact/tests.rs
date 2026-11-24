use std::sync::Mutex;

use http_body_util::BodyExt;
use rusqlite::Connection;
use tower::ServiceExt;

use super::{
    access::{local_api, owner},
    capabilities::{catalog, requested},
    *,
};

#[tokio::test]
async fn contact_router_does_not_expose_a_profile_avatar_proxy() {
    let endpoint = crate::test_support::identity::endpoint("alice.dhttp.net");
    let access = Arc::new(
        AccessService::load_from_db(
            "sqlite::memory:",
            endpoint.name(),
            owner(&endpoint).unwrap().subject_id(),
        )
        .await
        .unwrap(),
    );
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    let router = contact_router(
        Arc::new(Mutex::new(database(&profile).unwrap())),
        profile,
        endpoint,
        access,
    );
    for method in [Method::GET, Method::HEAD] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri("/std/contact/bob.dhttp.net/profile/avatar")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}

#[tokio::test]
async fn offered_capabilities_create_a_pinned_contact_before_granting_selected_operations() {
    let endpoint = crate::test_support::identity::endpoint("alice.dhttp.net");
    let owner = owner(&endpoint).unwrap();
    let access = Arc::new(
        AccessService::load_from_db("sqlite::memory:", endpoint.name(), owner.subject_id())
            .await
            .unwrap(),
    );
    let target = "bob.dhttp.net";
    let subject = "b".repeat(64);
    let selected = vec![
        json!({"id":"dev.dhttp.net/message#message","endpoints":[{"method":"PUT","path":"/std/comm/message"}]}),
    ];
    assert!(contact(&access, &endpoint, target).await.is_err());
    access::record_outbound(
        &access,
        target,
        &subject,
        json!({}),
        capabilities::offered(&selected),
        now() + 7 * 86400,
        now(),
    )
    .await
    .unwrap();
    access::apply_offered(
        &access,
        &endpoint,
        target,
        &subject,
        &capabilities::offered(&selected),
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
        json!({"/std/comm/message":{"allow":["PUT"],"review":[],"deny":[]}})
    );

    assert_eq!(
        access::apply_offered(
            &access,
            &endpoint,
            target,
            &"c".repeat(64),
            &capabilities::offered(&selected)
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
        access::apply_offered(
            &access,
            &endpoint,
            target,
            &subject,
            &capabilities::offered(&selected)
        )
        .await
        .unwrap_err()
        .0,
        409
    );
}

fn seed_request(db: &Connection, target: &str, state: &str, deadline: i64) {
    db.execute("INSERT INTO outbound_contact_request(target_name,state,application_id,description,requested,offered,local_deadline,next_attempt_at,created_at,updated_at)
        VALUES(?1,?2,lower(hex(randomblob(32))),'',?3,?3,?4,0,1,1)",params![target,state,r#"{"groups":[],"access":{}}"#,deadline]).unwrap();
}
fn legacy_db(profile: &IdentityProfile) -> Connection {
    std::fs::create_dir_all(profile.join(".volume/sys/contact")).unwrap();
    let db = Connection::open(profile.join(".volume/sys/contact/database")).unwrap();
    db.execute_batch(include_str!("../../fixtures/contact/v6/schema.sql"))
        .unwrap();
    db.execute_batch("PRAGMA user_version=6").unwrap();
    db
}
#[test]
fn expired_claims_cannot_resurrect_requests_and_database_reopens_without_private_permissions() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.join(".volume/sys/contact")).unwrap();
    let db = database(&profile).unwrap();
    seed_request(&db, "bob.dhttp.net", "queued", now() - 1);
    let old_lease = now() + 120;
    db.execute(
        "UPDATE outbound_contact_request SET lease_until=?1",
        [old_lease],
    )
    .unwrap();
    expire_requests(&db).unwrap();
    assert_eq!(
        db.execute(
            "DELETE FROM outbound_contact_request WHERE target_name='bob.dhttp.net' AND lease_until=?1",
            [old_lease]
        )
        .unwrap(),
        0
    );
    drop(db);
    let db = database(&profile).unwrap();
    assert_eq!(
        outbound_request(&db, "bob.dhttp.net").unwrap().unwrap()["status"],
        "expired"
    );
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
    std::fs::create_dir_all(profile.join(".volume/sys/contact")).unwrap();
    let db = Connection::open(profile.join(".volume/sys/contact/database")).unwrap();
    db.execute_batch("CREATE TABLE unrelated(id INTEGER)")
        .unwrap();
    assert!(database(&profile).is_err());
    db.execute_batch("DROP TABLE unrelated; PRAGMA user_version=1")
        .unwrap();
    assert!(database(&profile).is_err());
}

#[tokio::test]
async fn background_jobs_reuse_the_supplied_connection_and_release_transactions() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    let endpoint = crate::test_support::identity::endpoint("alice.dhttp.net");
    let access = Arc::new(AccessService::new(
        sea_orm::DatabaseConnection::default(),
        endpoint.name(),
    ));
    // No database exists at the profile path. Both rounds must use this connection.
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch(include_str!("../../../src/contact/sql/schema.sql"))
        .unwrap();
    let db = Arc::new(Mutex::new(db));
    for expected in 1..=2 {
        {
            let conn = db.lock().unwrap();
            seed_request(
                &conn,
                &format!("offline{expected}.dhttp.net"),
                "queued",
                now() - 1,
            );
        }
        contact_jobs(&profile, &endpoint, &access, &db)
            .await
            .unwrap();
        let conn = db
            .try_lock()
            .expect("background jobs must release the connection lock");
        assert!(conn.is_autocommit());
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM outbound_contact_request WHERE state='expired'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            expected
        );
    }
    {
        let conn = db.lock().unwrap();
        seed_request(&conn, "alice.dhttp.net", "queued", now() + 120);
    }
    // The missing local capability catalog fails delivery; its outcome must still
    // be written back through the supplied connection, with the claim released.
    contact_jobs(&profile, &endpoint, &access, &db)
        .await
        .unwrap();
    let conn = db
        .try_lock()
        .expect("outcome writes must release the connection lock");
    assert!(conn.is_autocommit());
    let outcome = conn.query_row(
        "SELECT next_attempt_at,lease_until,last_error FROM outbound_contact_request WHERE target_name='alice.dhttp.net'",
        [],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?, row.get::<_, Option<String>>(2)?)),
    ).unwrap();
    assert!(outcome.0 > now());
    assert_eq!(outcome.1, None);
    assert!(outcome.2.is_some_and(|error| !error.is_empty()));
    assert!(!profile.join(".volume/sys/contact/database").exists());
}

#[tokio::test]
async fn selected_permission_titles_resolve_to_actual_operations() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.join(".volume/sys/setting")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let db =
        crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    db.execute_batch("INSERT INTO package (has_lib,has_app,name, author, version) VALUES (1,0,'message', 'dev.dhttp.net', '1.0.0');
        INSERT INTO lib_capability VALUES ('message', 'read', 'read message'), ('message', 'history', 'read message');
        INSERT INTO lib_capability_operation VALUES ('message', 'read', 'GET', '/std/message'), ('message', 'read', 'HEAD', '/std/message'), ('message', 'history', 'GET', '/std/message/history');").unwrap();
    assert_eq!(
        catalog(&profile).unwrap(),
        json!({"dev.dhttp.net/message · read message":{"/message/message":["GET","HEAD"],"/message/message/history":["GET"]}})
    );
    let endpoint = crate::test_support::identity::endpoint("alice.dhttp.net");
    let operations = selected(
        &profile,
        &endpoint,
        &json!(["read message"]),
        endpoint.name(),
    )
    .await
    .unwrap();
    assert_eq!(
        requested(&operations),
        json!({"/message/message":["GET","HEAD"],"/message/message/history":["GET"]})
    );
    assert_eq!(
        selected(
            &profile,
            &endpoint,
            &json!(["read message", "read message"]),
            endpoint.name()
        )
        .await
        .unwrap_err()
        .0,
        400
    );
    assert_eq!(
        selected(
            &profile,
            &endpoint,
            &json!(["dev.dhttp.net/message#read"]),
            endpoint.name()
        )
        .await
        .unwrap_err()
        .0,
        409
    );

    let canonical = "dev.dhttp.net/message · read message";
    assert_eq!(
        selected(&profile, &endpoint, &json!([canonical]), endpoint.name())
            .await
            .unwrap(),
        operations
    );
    assert_eq!(
        selected(
            &profile,
            &endpoint,
            &json!([canonical, "read message"]),
            endpoint.name()
        )
        .await
        .unwrap_err()
        .0,
        400
    );
    db.execute_batch("INSERT INTO package (has_lib,has_app,name, author, version) VALUES (1,0,'archive', 'other.dhttp.net', '1.0.0');
        INSERT INTO lib_capability VALUES ('archive', 'read', 'read message');
        INSERT INTO lib_capability_operation VALUES ('archive', 'read', 'GET', '/archive/entries');").unwrap();
    assert_eq!(
        selected(
            &profile,
            &endpoint,
            &json!(["read message"]),
            endpoint.name()
        )
        .await
        .unwrap_err()
        .0,
        409,
        "legacy titles must not silently select multiple packages"
    );
    assert_eq!(
        selected(&profile, &endpoint, &json!([canonical]), endpoint.name())
            .await
            .unwrap(),
        operations
    );
    db.execute_batch("INSERT INTO package (has_lib,has_app,name,author,version)
        VALUES (1,1,'std/calendar','dev.genmeta.tech.dhttp.net','1.0.0');
        INSERT INTO lib_capability VALUES ('std/calendar','open','open calendar');
        INSERT INTO lib_capability_operation VALUES ('std/calendar','open','GET','/app/std/calendar');").unwrap();
    let calendar = selected(
        &profile,
        &endpoint,
        &json!(["open calendar"]),
        endpoint.name(),
    )
    .await
    .unwrap();
    assert_eq!(requested(&calendar), json!({"/app/calendar":["GET"]}));
    use tower::ServiceExt;
    let response = crate::routes::capabilities_router(profile.clone())
        .oneshot(
            Request::builder()
                .uri("/std/acl/permissions")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap(),
        catalog(&profile).unwrap()
    );
}

#[tokio::test]
async fn contact_routes_follow_acl_and_accept_an_authorized_manager() {
    let endpoint = crate::test_support::identity::endpoint("alice.dhttp.net");
    let manager = crate::test_support::identity::endpoint("bob.dhttp.net");
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.join(".volume/sys/access")).unwrap();
    let access_uri = format!(
        "sqlite://{}?mode=rwc",
        profile.join(".volume/sys/access/database").display()
    );
    let access = Arc::new(
        AccessService::load_from_db(
            &access_uri,
            endpoint.name(),
            owner(&endpoint).unwrap().subject_id(),
        )
        .await
        .unwrap(),
    );
    let db = database(&profile).unwrap();
    let router = contact_router(
        Arc::new(Mutex::new(db)),
        profile.clone(),
        endpoint.clone(),
        access.clone(),
    )
    .layer(axum::middleware::from_fn_with_state(
        access.clone(),
        crate::routes::authorize,
    ));
    let request = || {
        let mut request = Request::builder()
            .uri("/std/contact/list")
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
    access
        .set_policy(
            access_control::Method::Specified(Method::GET),
            "/std/contact/list",
            access_control::Effect::Allow,
            access_control::Grantee::Named,
        )
        .await
        .unwrap();
    assert_eq!(
        router.clone().oneshot(request()).await.unwrap().status(),
        StatusCode::OK
    );
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

#[tokio::test]
async fn contact_submission_without_capabilities_queues_without_contacting_the_peer() {
    let endpoint = crate::test_support::identity::endpoint("alice.dhttp.net");
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.join(".volume/sys/access")).unwrap();
    let access_uri = format!(
        "sqlite://{}?mode=rwc",
        profile.join(".volume/sys/access/database").display()
    );
    let access = Arc::new(
        AccessService::load_from_db(
            &access_uri,
            endpoint.name(),
            owner(&endpoint).unwrap().subject_id(),
        )
        .await
        .unwrap(),
    );
    let router = contact_router(
        Arc::new(Mutex::new(database(&profile).unwrap())),
        profile.clone(),
        endpoint.clone(),
        access.clone(),
    )
    .layer(axum::middleware::from_fn_with_state(
        access.clone(),
        crate::routes::authorize,
    ));
    let mut request = Request::builder().method(Method::POST).uri("/std/contact")
        .header("content-type", "application/json")
        .body(Body::from(json!({"from":"alice.dhttp.net","to":"offline.dhttp.net","description":"Hello","requested_capabilities":[],"offered_capabilities":[]}).to_string())).unwrap();
    request
        .extensions_mut()
        .insert(endpoint.local_authority().unwrap());
    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["status"], "pending");
    assert_eq!(body["delivery_status"], "queued");
    let queued = database(&profile).unwrap();
    assert_eq!(
        queued
            .query_row(
                "SELECT json_extract(requested,'$.access'),json_extract(offered,'$.access') FROM outbound_contact_request",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            )
            .unwrap(),
        ("{}".into(), "{}".into())
    );

    assert_eq!(body["name"], "offline.dhttp.net");
    assert!(body["subject_id"].is_null());
    assert!(
        access
            .find_contact_by_name("offline.dhttp.net")
            .await
            .is_err()
    );
    assert!(
        !profile.join(".volume/sys/setting/database").exists(),
        "no capability catalog is needed"
    );
    for uri in ["/std/contact?name=offline.dhttp.net", "/std/contact/list"] {
        let mut request = Request::builder().uri(uri).body(Body::empty()).unwrap();
        request
            .extensions_mut()
            .insert(endpoint.local_authority().unwrap());
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let value: Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        let entry = if uri == "/std/contact/list" {
            &value["items"][0]
        } else {
            &value
        };
        assert_eq!(entry["name"], "offline.dhttp.net");
        assert_eq!(entry["delivery_status"], "queued");
        assert!(entry["subject_id"].is_null());
    }
}

#[test]
fn stale_delivery_does_not_attach_to_a_different_contact_generation_or_subject() {
    let contact = json!({"id":2,"name":"bob.dhttp.net","subject_id":"current","status":"active","updated_at":4});
    for draft in [
        json!({"_contact_id":1,"_contact_subject":"current","status":"queued","updated_at":8}),
        json!({"_contact_id":2,"_contact_subject":"previous","status":"queued","updated_at":8}),
    ] {
        assert_eq!(project_contact(contact.clone(), &draft), contact);
    }
}

#[test]
fn contact_v1_upgrade_preserves_delivery_jobs_and_removes_only_saved_collection() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.join(".volume/sys/contact")).unwrap();
    let old = Connection::open(profile.join(".volume/sys/contact/database")).unwrap();
    old.execute_batch(include_str!("../../fixtures/contact/v1/schema.sql"))
        .unwrap();
    old.execute("INSERT INTO outbound_contact_requests(id,target_name,description,application_id,status,expired_after,delivery_deadline,next_attempt_at,created_at,updated_at) VALUES(41,'peer.dhttp.net','Keep request',?1,'pending',2000000000,2000000000,100,100,100)", ["a".repeat(64)]).unwrap();
    old.execute(
        "INSERT INTO saved_contacts VALUES('peer.dhttp.net',7,X'01',100)",
        [],
    )
    .unwrap();
    old.pragma_update(None, "user_version", 1).unwrap();
    drop(old);
    for _ in 0..2 {
        let db = database(&profile).unwrap();
        assert_eq!(
            db.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            8
        );
        assert_eq!(
            db.query_row(
                "SELECT json_extract(requested,'$.access'),json_extract(offered,'$.access') FROM outbound_contact_request",
                [],
                |row| Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?
                ))
            )
            .unwrap(),
            (Some("{}".into()), Some("{}".into()))
        );
        let job = outbound_request(&db, "peer.dhttp.net").unwrap().unwrap();
        assert_eq!(job["description"], "Keep request");
        assert_eq!(job["application_id"], "a".repeat(64));
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='saved_contacts'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(
            db.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
    }
}

#[test]
fn contact_v2_upgrade_preserves_bindings_sequence_and_pending_target_constraint() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.join(".volume/sys/contact")).unwrap();
    let old = Connection::open(profile.join(".volume/sys/contact/database")).unwrap();
    old.execute_batch(include_str!("../../fixtures/contact/v1/schema.sql"))
        .unwrap();
    old.execute_batch("ALTER TABLE outbound_contact_requests ADD COLUMN contact_id INTEGER;
        ALTER TABLE outbound_contact_requests ADD COLUMN contact_subject TEXT;
        DROP TABLE saved_contacts; PRAGMA user_version=2;
        INSERT INTO outbound_contact_requests(id,target_name,status,expired_after,delivery_deadline,next_attempt_at,created_at,updated_at,contact_id,contact_subject)
        VALUES(41,'peer.dhttp.net','pending',2000000000,2000000000,100,100,100,7,'subject');
        UPDATE sqlite_sequence SET seq=1000 WHERE name='outbound_contact_requests';")
        .unwrap();
    drop(old);
    for _ in 0..2 {
        let db = database(&profile).unwrap();
        assert_eq!(
            db.query_row(
                "SELECT contact_id,target_subject FROM outbound_contact_request WHERE target_name='peer.dhttp.net'",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            )
            .unwrap(),
            (7, "subject".into())
        );
        assert_eq!(
            outbound_request(&db, "peer.dhttp.net").unwrap().unwrap()["status"],
            "failed"
        );
        assert_eq!(db.query_row("SELECT count(*) FROM sqlite_master WHERE name IN ('outbound_contact_requests','outbound_contact_requests_pending_target')", [], |r|r.get::<_,i64>(0)).unwrap(),0);
    }
}

#[test]
fn contact_v3_migration_failure_rolls_back_rename_and_version() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.join(".volume/sys/contact")).unwrap();
    let old = Connection::open(profile.join(".volume/sys/contact/database")).unwrap();
    old.execute_batch(include_str!("../../fixtures/contact/v1/schema.sql"))
        .unwrap();
    // A pre-existing target index makes the migration fail after ALTER TABLE.
    old.execute_batch("CREATE UNIQUE INDEX outbound_contact_request_pending_target ON outbound_contact_requests(target_name); PRAGMA user_version=1;
        INSERT INTO saved_contacts VALUES('peer.dhttp.net',7,X'01',100);")
        .unwrap();
    assert!(database(&profile).is_err());
    assert_eq!(
        old.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert!(
        old.prepare("SELECT id FROM outbound_contact_requests")
            .is_ok()
    );
    assert_eq!(
        old.query_row("SELECT count(*) FROM saved_contacts", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(old.query_row("SELECT count(*) FROM pragma_table_info('outbound_contact_requests') WHERE name IN ('contact_id','contact_subject')", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
}

#[tokio::test]
async fn outbound_submission_requires_both_verified_owner_name_and_subject() {
    let endpoint = crate::test_support::identity::endpoint("alice.dhttp.net");
    let verified_owner = owner(&endpoint).unwrap();
    let access = Arc::new(
        AccessService::load_from_db(
            "sqlite::memory:",
            endpoint.name(),
            verified_owner.subject_id(),
        )
        .await
        .unwrap(),
    );
    access
        .set_policy(
            access_control::Method::Specified(Method::POST),
            "/std/contact",
            access_control::Effect::Allow,
            access_control::Grantee::Named,
        )
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    for visitor in [
        access_control::Visitor::new(
            "manager.dhttp.net",
            access_control::SubjectId::new(b"manager").unwrap(),
        ),
        access_control::Visitor::new(
            endpoint.name(),
            access_control::SubjectId::new(b"impostor").unwrap(),
        ),
    ] {
        let mut request = Request::builder().method(Method::POST).uri("/std/contact").header("content-type", "application/json").body(Body::from(json!({"from":endpoint.name(),"to":"bob.dhttp.net","description":"","requested_capabilities":[],"offered_capabilities":[]}).to_string())).unwrap();
        request.extensions_mut().insert(visitor);
        assert_eq!(
            dispatch(
                Arc::new(Mutex::new(Connection::open_in_memory().unwrap())),
                profile.clone(),
                endpoint.clone(),
                access.clone(),
                request
            )
            .await
            .unwrap_err()
            .0,
            403
        );
    }
    assert!(!profile.join(".volume/sys/contact/.db").exists());
}

#[tokio::test]
async fn queued_legacy_capabilities_fail_without_network_or_new_grants() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    let endpoint = crate::test_support::identity::endpoint("alice.dhttp.net");
    let access = Arc::new(
        AccessService::load_from_db(
            "sqlite::memory:",
            endpoint.name(),
            owner(&endpoint).unwrap().subject_id(),
        )
        .await
        .unwrap(),
    );
    let db = legacy_db(&profile);
    db.execute("INSERT INTO outbound_contact_request(target_name,application_id,requested_capabilities,status,expired_after,delivery_deadline,next_attempt_at,created_at,updated_at) VALUES('bob.dhttp.net',?1,'[\"Receive messages\"]','queued',?2,?2,0,0,0)", params!["a".repeat(64), now()+120]).unwrap();
    drop(db);
    let db = Arc::new(Mutex::new(database(&profile).unwrap()));
    contact_jobs(&profile, &endpoint, &access, &db)
        .await
        .unwrap();
    {
        let conn = db.lock().unwrap();
        let (status, error, lease): (String, String, Option<i64>) = conn
            .query_row(
                "SELECT state,last_error,lease_until FROM outbound_contact_request",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(status, "failed");
        assert!(error.contains("submit a new request"));
        assert_eq!(lease, None);
    }
    assert!(access.find_contact_by_name("bob.dhttp.net").await.is_err());
}

#[test]
fn v3_migration_preserves_unknown_snapshots_without_catalog_backfill() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    let db = legacy_db(&profile);
    db.execute_batch("ALTER TABLE outbound_contact_request DROP COLUMN review_id; ALTER TABLE outbound_contact_request DROP COLUMN review_subject; ALTER TABLE outbound_contact_request DROP COLUMN review_method; ALTER TABLE outbound_contact_request DROP COLUMN review_path; ALTER TABLE outbound_contact_request DROP COLUMN requested_access; ALTER TABLE outbound_contact_request DROP COLUMN offers; PRAGMA user_version=3;").unwrap();
    db.execute("INSERT INTO outbound_contact_request(target_name,requested_capabilities,status,expired_after,delivery_deadline,next_attempt_at,created_at,updated_at) VALUES('bob.dhttp.net','[\"Receive messages\"]','queued',?1,?1,0,1,1)", [now()+120]).unwrap();
    drop(db);
    for _ in 0..2 {
        let db = database(&profile).unwrap();
        assert_eq!(
            db.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            8
        );
        assert_eq!(
            db.query_row(
                "SELECT json_extract(requested,'$.access'),json_extract(offered,'$.access') FROM outbound_contact_request",
                [],
                |row| Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?
                ))
            )
            .unwrap(),
            (Some("{}".into()),Some("{}".into()))
        );
    }
}

#[test]
fn v4_contact_migration_preserves_review_tracking_across_reopen() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    let db = legacy_db(&profile);
    db.execute_batch("ALTER TABLE outbound_contact_request DROP COLUMN review_id; ALTER TABLE outbound_contact_request DROP COLUMN review_subject; ALTER TABLE outbound_contact_request DROP COLUMN review_method; ALTER TABLE outbound_contact_request DROP COLUMN review_path; PRAGMA user_version=4;").unwrap();
    db.execute("INSERT INTO outbound_contact_request(target_name,status,expired_after,delivery_deadline,next_attempt_at,created_at,updated_at) VALUES('bob.dhttp.net','queued',?1,?1,0,1,1)",[now()+3600]).unwrap();
    db.execute_batch(include_str!(
        "../../../src/contact/sql/migrations/v5_add_review_tracking.sql"
    ))
    .unwrap();
    db.execute_batch("PRAGMA user_version=5").unwrap();
    db.execute("UPDATE outbound_contact_request SET application_id=lower(hex(randomblob(32))),review_id=27,review_subject='verified-peer',review_method='POST',review_path='/std/contact'",[]).unwrap();
    drop(db);
    let db = database(&profile).unwrap();
    let review:(i64,String,String,String)=db.query_row("SELECT review_id,target_subject,CASE state WHEN 'queued' THEN 'POST' END,'/std/contact' FROM outbound_contact_request",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
    assert_eq!(
        review,
        (
            27,
            "verified-peer".into(),
            "POST".into(),
            "/std/contact".into()
        )
    );
}

#[tokio::test]
async fn completed_contact_applications_never_reenter_the_delivery_queue() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    let endpoint = crate::test_support::identity::endpoint("alice.dhttp.net");
    // Any attempted local contact lookup would fail against this empty service.
    let access = Arc::new(AccessService::new(
        sea_orm::DatabaseConnection::default(),
        endpoint.name(),
    ));
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch(include_str!("../../../src/contact/sql/schema.sql"))
        .unwrap();
    for state in ["denied", "expired", "failed"] {
        seed_request(&db, &format!("{state}.dhttp.net"), state, now() + 3600);
    }
    seed_request(&db, "pending.dhttp.net", "pending", now() - 1);
    let db = Arc::new(Mutex::new(db));
    for _ in 0..3 {
        contact_jobs(&profile, &endpoint, &access, &db)
            .await
            .unwrap();
    }
    let db = db.lock().unwrap();
    assert_eq!(db.query_row("SELECT count(*) FROM outbound_contact_request WHERE lease_until IS NULL AND updated_at=1",[],|r|r.get::<_,i64>(0)).unwrap(),3);
    assert_eq!(
        db.query_row(
            "SELECT state FROM outbound_contact_request WHERE target_name='pending.dhttp.net'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "expired"
    );
    assert!(db.is_autocommit());
}

fn group_entry() -> Value {
    json!({"kind":"group","name":"alice~#dev","bound_subject":"a".repeat(64)})
}
fn group_service() -> (String, String) {
    (
        "dev.genmeta.tech.dhttp.net/std/comm/group".into(),
        "c".repeat(64),
    )
}

#[tokio::test]
async fn group_directory_binds_the_name_and_preserves_its_alias() {
    let endpoint = crate::test_support::identity::endpoint("alice.dhttp.net");
    let visitor = owner(&endpoint).unwrap();
    let access =
        AccessService::load_from_db("sqlite::memory:", endpoint.name(), visitor.subject_id())
            .await
            .unwrap();
    let service = group_service();
    assert!(
        directory::put(&access, &group_entry(), None, 1)
            .await
            .is_err()
    );
    directory::put(&access, &group_entry(), Some(&service), 1)
        .await
        .unwrap();
    directory::patch(&access, "alice~#dev", &json!({"alias":"Work"}), 2)
        .await
        .unwrap();
    let saved = directory::put(&access, &group_entry(), Some(&service), 3)
        .await
        .unwrap();
    assert_eq!(saved["alias"], "Work");
    let mut conflict = group_entry();
    conflict["bound_subject"] = json!("b".repeat(64));
    assert_eq!(
        directory::put(&access, &conflict, Some(&service), 4)
            .await
            .unwrap_err()
            .0,
        409
    );
    conflict["name"] = json!("alice~#other");
    conflict["bound_subject"] = saved["bound_subject"].clone();
    assert!(
        directory::put(&access, &conflict, Some(&service), 5)
            .await
            .unwrap()["alias"]
            .is_null()
    );
}

#[tokio::test]
async fn directory_routes_merge_filter_paginate_and_require_verified_owner() {
    let endpoint = crate::test_support::identity::endpoint("alice.dhttp.net");
    let visitor = owner(&endpoint).unwrap();
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.join(".volume/sys/access")).unwrap();
    let access_uri = format!(
        "sqlite://{}?mode=rwc",
        profile.join(".volume/sys/access/database").display()
    );
    let access = Arc::new(
        AccessService::load_from_db(&access_uri, endpoint.name(), visitor.subject_id())
            .await
            .unwrap(),
    );
    let service = group_service();
    directory::put(&access, &group_entry(), Some(&service), 1)
        .await
        .unwrap();
    assert!(access.find_contact_by_name("alice~#dev").await.is_ok());
    let router = contact_router(
        Arc::new(Mutex::new(database(&profile).unwrap())),
        profile.clone(),
        endpoint,
        access,
    );
    let response = router
        .clone()
        .oneshot(
            Request::get("/std/contact/list?kind=all&page_size=1&sort=name&order=asc")
                .extension(visitor.clone())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let value: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(status, StatusCode::OK, "{value}");
    assert_eq!(value["total"], 1);
    assert_eq!(value["items"][0]["kind"], "group");
    let response = router
        .clone()
        .oneshot(
            Request::get("/std/contact/list?kind=all&search=missing")
                .extension(visitor.clone())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let value: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(value["total"], 0);

    // One group, 101 people and one unbound draft share the same timestamp.
    let acl = Connection::open(profile.join(".volume/sys/access/database")).unwrap();
    acl.execute_batch(
        "WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM n WHERE x<100)
        INSERT INTO contacts(name,subject_id,class,status,expired_after,created_at,updated_at)
        SELECT printf('peer%03d.dhttp.net',x),x'6162','Human',2,9999999999,1,1 FROM n",
    )
    .unwrap();
    let db = database(&profile).unwrap();
    seed_request(&db, "peer098a.dhttp.net", "queued", now() + 600);
    seed_request(&db, "peer100.dhttp.net", "pending", now() + 600);
    acl.execute(
        "UPDATE contacts SET subject_id=x'00ff' WHERE name='peer100.dhttp.net'",
        [],
    )
    .unwrap();
    db.execute(
        "UPDATE outbound_contact_request SET contact_id=?1,target_subject='hex:00ff'
        WHERE target_name='peer100.dhttp.net'",
        [acl.query_row(
            "SELECT id FROM contacts WHERE name='peer100.dhttp.net'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()],
    )
    .unwrap();
    drop(db);
    drop(acl);
    let list = |query: &str| {
        let req = Request::get(format!("/std/contact/list?kind=all&{query}"))
            .extension(visitor.clone())
            .body(Body::empty())
            .unwrap();
        let service = router.clone();
        async move {
            let response = service.oneshot(req).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            serde_json::from_slice::<Value>(
                &response.into_body().collect().await.unwrap().to_bytes(),
            )
            .unwrap()
        }
    };
    let result = list("page=2&page_size=100&sort=updated_at&order=asc").await;
    assert_eq!(result["total"], 103);
    let items = result["items"].as_array().unwrap();
    assert_eq!(
        items
            .iter()
            .map(|item| item["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "peer098a.dhttp.net",
            "peer099.dhttp.net",
            "peer100.dhttp.net"
        ]
    );
    assert!(items[0]["id"].is_null());
    assert_eq!(items[2]["subject_id"], "hex:00ff");
    assert_eq!(items[2]["delivery_status"], "pending");
    let reverse = list("page_size=3&sort=updated_at&order=desc").await;
    assert_eq!(reverse["items"][0], items[2]);
    assert_eq!(reverse["items"][2], items[0]);
    let empty = list("page=3&page_size=100").await;
    assert_eq!(empty["total"], 103);
    assert_eq!(empty["items"], json!([]));
    let response = router
        .clone()
        .oneshot(
            Request::get("/std/contact?name=alice~%23dev")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = router
        .oneshot(
            Request::put("/std/contact")
                .extension(visitor)
                .header(
                    "pishoo-source-lib",
                    "dev.genmeta.tech.dhttp.net/std/comm/group",
                )
                .body(Body::from(group_entry().to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[test]
fn v6_upgrade_removes_title_entries_and_preserves_groups_and_grants() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    let db = legacy_db(&profile);
    db.execute_batch("PRAGMA user_version=6;
        INSERT INTO contact_entries(name,kind,alias,bound_name,bound_subject,source_revision,created_at,source_updated_at,updated_at)
        VALUES('student@mit.edu','title','Student','bob.dhttp.net','subject',1,1,1,1);
        INSERT INTO contact_entries(name,kind,alias,nickname,host,host_owner_hash,member_joined_seq,local_state,source_revision,created_at,source_updated_at,updated_at)
        VALUES('alice~#dev','group','Private','dev','alice.dhttp.net',lower(hex(zeroblob(32))),42,'active',2,1,1,2);
        INSERT INTO directory_service_grants SELECT 'service',lower(hex(zeroblob(32))),kind FROM
        (SELECT 'group' AS kind UNION ALL SELECT 'name' UNION ALL SELECT 'title');").unwrap();
    assert!(database(&profile).is_err());
    db.execute_batch(include_str!(
        "../../../src/contact/sql/migrations/v7_simplify_contacts.sql"
    ))
    .unwrap();
    assert_eq!(
        db.query_row(
            "SELECT alias FROM contact_entries WHERE name='alice~#dev'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "Private"
    );
    assert!(
        db.prepare("SELECT bound_name FROM contact_entries")
            .is_err()
    );
}

#[tokio::test]
async fn receipts_are_scoped_to_the_application_and_verified_applicant() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("receiver")).unwrap();
    let endpoint = crate::test_support::identity::endpoint("receiver.dhttp.net");
    let receiver = owner(&endpoint).unwrap();
    let peer = owner(&crate::test_support::identity::endpoint(
        "applicant.dhttp.net",
    ))
    .unwrap();
    let access = Arc::new(
        AccessService::load_from_db("sqlite::memory:", endpoint.name(), receiver.subject_id())
            .await
            .unwrap(),
    );
    let router = contact_router(
        Arc::new(Mutex::new(database(&profile).unwrap())),
        profile,
        endpoint.clone(),
        access.clone(),
    );
    let id = "a".repeat(64);
    let send=Request::post("/std/contact").extension(peer.clone()).header("content-type","application/json")
        .body(Body::from(json!({"from":peer.name(),"to":endpoint.name(),"application_id":id,"class":"Human","description":"Hi",
        "requested_access":{},"offers":{}}).to_string())).unwrap();
    assert_eq!(router.clone().oneshot(send).await.unwrap().status(), 201);
    let lookup = |app: &str, visitor: access_control::Visitor| {
        Request::get(format!("/std/contact?application_id={app}"))
            .extension(visitor)
            .body(Body::empty())
            .unwrap()
    };
    for expected in ["pending", "active"] {
        let response = router
            .clone()
            .oneshot(lookup(&id, peer.clone()))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let doc: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 8192)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(doc["status"], expected);
        assert_eq!(doc["application_id"], id);
        assert_eq!(doc["name"], endpoint.name());
        if expected == "pending" {
            local_api(
                &access,
                &endpoint,
                Method::PATCH,
                &format!("/std/contact?name={}", peer.name()),
                Some(&json!({"status":"active"})),
            )
            .await
            .unwrap();
        }
    }
    // Both the application and its contact row still carry the original deadline.
    use sea_orm::ConnectionTrait;
    access
        .database()
        .execute_unprepared(
            "UPDATE contacts SET created_at=1,updated_at=2,expired_after=3;
         UPDATE contact_applications SET received_at=1,expired_after=3;",
        )
        .await
        .unwrap();
    let accepted = access::receipt(&access, &receiver, &peer, &id)
        .await
        .unwrap();
    assert_eq!(accepted["status"], "active");
    assert_eq!(accepted["valid_before"], 3);
    access
        .database()
        .execute_unprepared("DELETE FROM contacts")
        .await
        .unwrap();
    let removed = access::receipt(&access, &receiver, &peer, &id)
        .await
        .unwrap();
    assert_eq!(removed["status"], "revoked");
    let other = access_control::Visitor::new(
        peer.name(),
        access_control::SubjectId::new(b"different-applicant-subject").unwrap(),
    );
    assert_eq!(
        router
            .clone()
            .oneshot(lookup(&id, other))
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        router
            .oneshot(lookup(&"b".repeat(64), peer))
            .await
            .unwrap()
            .status(),
        404
    );
}
