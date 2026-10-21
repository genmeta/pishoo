use std::sync::Mutex;

use rusqlite::Connection;
use tower::ServiceExt;

use super::{
    access::{local_api, owner},
    capabilities::{catalog, requested},
    *,
};

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

#[test]
fn expired_claims_cannot_resurrect_requests_and_database_reopens_without_private_permissions() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.join(".volume/sys/contact")).unwrap();
    let db = database(&profile).unwrap();
    db.execute("INSERT INTO outbound_contact_request(target_name,status,expired_after,delivery_deadline,next_attempt_at,lease_until,created_at,updated_at) VALUES('bob.dhttp.net','queued',?1,?1,0,?2,0,0)", params![now()-1,now()+120]).unwrap();
    let old_lease = now() + 120;
    expire_requests(&db).unwrap();
    assert_eq!(
        db.execute(
            "UPDATE outbound_contact_request SET status='active' WHERE id=1 AND lease_until=?1",
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
    std::fs::create_dir_all(profile.join(".volume/sys/contact")).unwrap();
    let db = Connection::open(profile.join(".volume/sys/contact/.db")).unwrap();
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
    let endpoint = crate::test_identity::endpoint("alice.dhttp.net");
    let access = Arc::new(AccessService::new(
        sea_orm::DatabaseConnection::default(),
        endpoint.name(),
    ));
    // No database exists at the profile path. Both rounds must use this connection.
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch(include_str!("../../src/contact/contact.sql"))
        .unwrap();
    let db = Arc::new(Mutex::new(db));
    for expected in 1..=2 {
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO outbound_contact_request(target_name,status,expired_after,delivery_deadline,next_attempt_at,created_at,updated_at) VALUES('bob.dhttp.net','queued',?1,?1,0,0,0)",
                [now() - 1],
            ).unwrap();
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
                "SELECT count(*) FROM outbound_contact_request WHERE status='failed'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            expected
        );
    }
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO outbound_contact_request(target_name,status,application_id,expired_after,delivery_deadline,next_attempt_at,created_at,updated_at) VALUES('alice.dhttp.net','queued',?2,?1,?1,0,0,0)",
            params![now() + 120, "a".repeat(64)],
        ).unwrap();
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
        "SELECT attempt_count,lease_until,error_message FROM outbound_contact_request WHERE target_name='alice.dhttp.net'",
        [],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?, row.get::<_, Option<String>>(2)?)),
    ).unwrap();
    assert_eq!(outcome.0, 1);
    assert_eq!(outcome.1, None);
    assert!(outcome.2.is_some_and(|error| !error.is_empty()));
    assert!(!profile.join(".volume/sys/contact/.db").exists());
}

#[tokio::test]
async fn selected_permission_titles_resolve_to_actual_operations() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.join(".volume/sys/setting")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let db =
        crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    db.execute_batch("INSERT INTO lib (name, author, version) VALUES ('message', 'dev.dhttp.net', '1.0.0');
        INSERT INTO lib_capability VALUES ('message', 'read', 'read message'), ('message', 'history', 'read message');
        INSERT INTO lib_capability_operation VALUES ('message', 'read', 'GET', '/std/message'), ('message', 'read', 'HEAD', '/std/message'), ('message', 'history', 'GET', '/std/message/history');").unwrap();
    assert_eq!(
        catalog(&profile).unwrap(),
        json!({"dev.dhttp.net/message · read message":{"/message/message":["GET","HEAD"],"/message/message/history":["GET"]}})
    );
    let endpoint = crate::test_identity::endpoint("alice.dhttp.net");
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
    db.execute_batch("INSERT INTO lib (name, author, version) VALUES ('archive', 'other.dhttp.net', '1.0.0');
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
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.join(".volume/sys/contact")).unwrap();
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
    assert!(!profile.join(".volume/sys/contact/.db").exists());
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
    assert!(profile.join(".volume/sys/contact/.db").exists());
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
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.join(".volume/sys/contact")).unwrap();
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
    assert_eq!(body["name"], "offline.dhttp.net");
    assert!(body["subject_id"].is_null());
    assert!(
        access
            .find_contact_by_name("offline.dhttp.net")
            .await
            .is_err()
    );
    assert!(
        !profile.join(".volume/sys/setting/.db").exists(),
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
    let old = Connection::open(profile.join(".volume/sys/contact/.db")).unwrap();
    old.execute_batch(include_str!("../fixtures/contact-v1.sql"))
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
            3
        );
        let job = db.query_row("SELECT id,description,status,contact_id,contact_subject FROM outbound_contact_request", [], |row| Ok((row.get::<_,i64>(0)?, row.get::<_,String>(1)?, row.get::<_,String>(2)?, row.get::<_,Option<i64>>(3)?, row.get::<_,Option<String>>(4)?))).unwrap();
        assert_eq!(
            job,
            (41, "Keep request".into(), "pending".into(), None, None)
        );
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
    let old = Connection::open(profile.join(".volume/sys/contact/.db")).unwrap();
    old.execute_batch(include_str!("../fixtures/contact-v1.sql"))
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
                "SELECT contact_id,contact_subject FROM outbound_contact_request WHERE id=41",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            )
            .unwrap(),
            (7, "subject".into())
        );
        assert!(db.execute("INSERT INTO outbound_contact_request(target_name,status,expired_after,delivery_deadline,next_attempt_at,created_at,updated_at) VALUES('peer.dhttp.net','queued',2000000000,2000000000,100,100,100)", []).is_err());
        assert_eq!(
            db.query_row(
                "SELECT seq FROM sqlite_sequence WHERE name='outbound_contact_request'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            1000
        );
        assert_eq!(db.query_row("SELECT count(*) FROM sqlite_master WHERE name IN ('outbound_contact_requests','outbound_contact_requests_pending_target')", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
    }
    let db = database(&profile).unwrap();
    db.execute("INSERT INTO outbound_contact_request(target_name,status,expired_after,delivery_deadline,next_attempt_at,created_at,updated_at) VALUES('other.dhttp.net','queued',2000000000,2000000000,100,100,100)", []).unwrap();
    assert!(db.last_insert_rowid() > 1000);
}

#[test]
fn contact_v3_migration_failure_rolls_back_rename_and_version() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.join(".volume/sys/contact")).unwrap();
    let old = Connection::open(profile.join(".volume/sys/contact/.db")).unwrap();
    old.execute_batch(include_str!("../fixtures/contact-v1.sql"))
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
