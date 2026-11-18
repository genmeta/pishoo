use base64::Engine;
use http_body_util::BodyExt;
use tower::ServiceExt;

use super::*;

#[path = "../../common/system.rs"]
mod common;
use common::*;

#[tokio::test]
async fn names_are_canonical_and_owner_changes_cannot_replace_an_administrator() {
    let (_root, _home, db) = database();
    let db = db.lock().await;
    let name = storage::canonical(" ADMIN.Name~ ").unwrap();
    storage::put_admin(&db, &name, &[b'a'; 64]).unwrap();
    storage::put_admin(
        &db,
        &storage::canonical("admin.name.dhttp.net").unwrap(),
        &[b'a'; 64],
    )
    .unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM administrator", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert!(storage::put_admin(&db, &name, &[b'b'; 64]).is_err());
}

#[tokio::test]
async fn administrator_authorization_uses_only_identity_bindings() {
    let (_root, _home, db) = database();
    let local = visitor("local.name", b'l');
    let remote = visitor("external.name", b'e');
    let stranger = visitor("stranger.name", b's');
    let db = db.lock().await;
    storage::put_admin(&db, local.name(), local.subject_id().as_bytes()).unwrap();
    storage::put_admin(&db, remote.name(), remote.subject_id().as_bytes()).unwrap();
    storage::authorize(&db, &local, &remote).unwrap();
    storage::authorize(&db, &remote, &local).unwrap();
    for (target, caller) in [
        (&stranger, &remote),
        (&local, &stranger),
        (&local, &visitor("external.name", b'x')),
    ] {
        assert!(matches!(
            storage::authorize(&db, target, caller),
            Err(Error::Denied)
        ));
    }
    db.execute("DELETE FROM administrator WHERE name=?1", [remote.name()])
        .unwrap();
    assert!(matches!(
        storage::authorize(&db, &local, &remote),
        Err(Error::Denied)
    ));
}

#[tokio::test]
async fn http_routes_require_a_registered_administrator_and_validate_native_requests() {
    let (_root, home, db) = database();
    let endpoint = crate::test_support::identity::endpoint("owner.name");
    let local = endpoint.local_authority().unwrap();
    let owner = identity(local.name(), local.certificates()).unwrap();
    let (commands, mut receiver) = mpsc::channel(8);
    let app = router(home.clone(), db.clone(), commands).layer(axum::Extension(endpoint.clone()));
    for anonymous in [true, false] {
        assert_eq!(
            app.clone()
                .oneshot(request(
                    &endpoint,
                    Method::GET,
                    "/sys/info",
                    Body::empty(),
                    anonymous
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        db.lock()
            .await
            .query_row("SELECT count(*) FROM administrator", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    register_host(&db, &users::profile(&home, endpoint.name()), &endpoint)
        .await
        .unwrap();
    for (method, path, anonymous, status) in [
        (Method::GET, "/sys/info", false, StatusCode::OK),
        (Method::GET, "/sys", false, StatusCode::NOT_FOUND),
        (Method::GET, "/sys/info", true, StatusCode::FORBIDDEN),
        (Method::GET, "/sys/admin", true, StatusCode::FORBIDDEN),
        (Method::POST, "/sys/user", true, StatusCode::FORBIDDEN),
        (Method::POST, "/sys/restart", true, StatusCode::FORBIDDEN),
        (
            Method::PATCH,
            "/sys/info",
            false,
            StatusCode::METHOD_NOT_ALLOWED,
        ),
        (Method::GET, "/sys/unknown", false, StatusCode::NOT_FOUND),
        (
            Method::GET,
            "/sys/version?extra=1",
            false,
            StatusCode::BAD_REQUEST,
        ),
        (
            Method::DELETE,
            "/sys/admin?name=a&name=b",
            false,
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let response = app
            .clone()
            .oneshot(request(&endpoint, method, path, Body::empty(), anonymous))
            .await
            .unwrap();
        assert_eq!(response.status(), status, "{path}");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    }
    let mut forged = Request::builder()
        .uri("/sys/version")
        .header("pishoo-viewer-name", owner.name())
        .body(Body::empty())
        .unwrap();
    forged.extensions_mut().insert(owner);
    assert_eq!(
        app.clone().oneshot(forged).await.unwrap().status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    let response = app
        .clone()
        .oneshot(request(
            &endpoint,
            Method::POST,
            "/sys/reload",
            Body::empty(),
            false,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert!(receiver.try_recv().is_err());
    response.into_body().collect().await.unwrap();
    assert_eq!(receiver.recv().await, Some("reload"));
    db.lock()
        .await
        .execute("DELETE FROM administrator", [])
        .unwrap();
    register_host(&db, &users::profile(&home, endpoint.name()), &endpoint)
        .await
        .unwrap();
    assert_eq!(
        app.oneshot(request(
            &endpoint,
            Method::GET,
            "/sys/info",
            Body::empty(),
            false
        ))
        .await
        .unwrap()
        .status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn imported_users_keep_device_administration_separate_from_identity_access() {
    let (root, home, database) = database();
    let fixtures = tempfile::tempdir().unwrap();
    crate::test_support::credentials::generate(
        fixtures.path(),
        &[
            ("first.name", "first.name.dhttp.net"),
            ("second.name", "second.name.dhttp.net"),
        ],
    );
    let trust = std::fs::read(fixtures.path().join("ca.der")).unwrap();
    let remote = visitor("remote.name", b'a');
    let mut db = database.lock().await;
    storage::put_admin(&db, "support.name.dhttp.net", &[b'i'; 64]).unwrap();
    storage::put_admin(&db, remote.name(), remote.subject_id().as_bytes()).unwrap();
    let result = users::create(
        &home,
        &mut db,
        imported_body(fixtures.path(), "first.name"),
        &trust,
    )
    .await
    .unwrap();
    assert_eq!(result["created"], true);
    let first = users::profile(&home, "first.name.dhttp.net");
    let certs = first.load_certs().await.unwrap();
    let owner = identity(first.name(), &certs).unwrap();
    assert!(!storage::administrator(&db, &owner).unwrap());
    storage::put_admin(&db, owner.name(), owner.subject_id().as_bytes()).unwrap();
    let access = crate::server::access_for_system(&first, owner.subject_id())
        .await
        .unwrap();
    assert!(access.find_contact_by_name(remote.name()).await.is_err());
    assert!(
        access
            .find_contact_by_name("support.name.dhttp.net")
            .await
            .is_err()
    );
    assert!(access.find_contact_by_name(owner.name()).await.is_err());
    let authorization = access
        .auth(
            access_control::Headers {
                method: Method::DELETE,
                path: "/anything".into(),
                fields: Default::default(),
            },
            Some(remote.name()),
            Some(remote.subject_id()),
        )
        .await
        .unwrap();
    assert!(matches!(authorization, access_control::AuthResult::Denied));
    assert!(matches!(
        access
            .auth(
                access_control::Headers {
                    method: Method::DELETE,
                    path: "/anything".into(),
                    fields: Default::default(),
                },
                Some(owner.name()),
                Some(owner.subject_id()),
            )
            .await
            .unwrap(),
        access_control::AuthResult::Allowed
    ));
    let second_body = imported_body(fixtures.path(), "second.name");
    users::create(&home, &mut db, second_body.clone(), &trust)
        .await
        .unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM administrator", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        3
    );
    let second = users::profile(&home, "second.name.dhttp.net");
    let second_subject = identity(second.name(), &second.load_certs().await.unwrap()).unwrap();
    let second_access = crate::server::access_for_system(&second, second_subject.subject_id())
        .await
        .unwrap();
    assert!(
        second_access
            .find_contact_by_name(owner.name())
            .await
            .is_err()
    );
    assert!(
        second_access
            .find_contact_by_name(remote.name())
            .await
            .is_err()
    );
    assert!(!storage::administrator(&db, &second_subject).unwrap());
    let key_before = std::fs::read(second.key_path()).unwrap();
    let retry = users::create(&home, &mut db, second_body, &trust)
        .await
        .unwrap();
    assert_eq!(retry["created"], false);
    assert_eq!(std::fs::read(second.key_path()).unwrap(), key_before);
    users::delete(&home, &mut db, second.name()).await.unwrap();
    assert!(!second.path().exists());
    assert!(first.path().exists());
    assert!(root.path().join(".system/database").exists());
}

#[tokio::test]
async fn importing_an_existing_administrator_preserves_membership_without_creating_contacts() {
    let (_root, home, database) = database();
    let fixtures = tempfile::tempdir().unwrap();
    crate::test_support::credentials::generate(
        fixtures.path(),
        &[("admin.name", "admin.name.dhttp.net")],
    );
    let trust = std::fs::read(fixtures.path().join("ca.der")).unwrap();
    let original = users::profile(
        &DhttpHome::new(fixtures.path().into()),
        "admin.name.dhttp.net",
    );
    let owner = identity(original.name(), &original.load_certs().await.unwrap()).unwrap();
    let mut db = database.lock().await;
    storage::put_admin(&db, owner.name(), owner.subject_id().as_bytes()).unwrap();
    users::create(
        &home,
        &mut db,
        imported_body(fixtures.path(), "admin.name"),
        &trust,
    )
    .await
    .unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM administrator", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    let profile = users::profile(&home, owner.name());
    let access = crate::server::access_for_system(&profile, owner.subject_id())
        .await
        .unwrap();
    assert!(access.find_contact_by_name(owner.name()).await.is_err());
}

#[tokio::test]
async fn invalid_imports_preserve_device_authority_and_existing_material() {
    let (_root, home, database) = database();
    let fixtures = tempfile::tempdir().unwrap();
    crate::test_support::credentials::generate(
        fixtures.path(),
        &[
            ("first.name", "first.name.dhttp.net"),
            ("other.name", "other.name.dhttp.net"),
        ],
    );
    let trust = std::fs::read(fixtures.path().join("ca.der")).unwrap();
    let body = imported_body(fixtures.path(), "first.name");
    let mut db = database.lock().await;
    for field in ["name", "private_key", "ocsp", "certificate", "unknown"] {
        let mut invalid = body.clone();
        invalid[field] = if field == "private_key" {
            imported_body(fixtures.path(), "other.name")["private_key"].clone()
        } else {
            "invalid".into()
        };
        assert!(
            users::create(&home, &mut db, invalid, &trust)
                .await
                .is_err(),
            "{field}"
        );
        assert!(!home.join("first.name").exists());
    }
    assert!(
        users::create(&home, &mut db, body, dhttp::trust::DHTTP_ROOT_CA_DER)
            .await
            .is_err()
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM managed_user", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn committed_filesystem_operations_recover_without_overwriting_identities() {
    let (_root, home, database) = database();
    let mut db = database.lock().await;
    let pending = home.join(".system/pending/alice.name");
    std::fs::create_dir_all(pending.join("ssl")).unwrap();
    let endpoint = crate::test_support::identity::endpoint("alice.name");
    let local = endpoint.local_authority().unwrap();
    let pem = local
        .certificates()
        .iter()
        .map(|cert| {
            format!(
                "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
                base64::engine::general_purpose::STANDARD.encode(cert.as_ref())
            )
        })
        .collect::<String>();
    std::fs::write(pending.join("ssl/fullchain.crt"), pem).unwrap();
    std::fs::write(pending.join("marker"), "original").unwrap();
    db.execute(
        "INSERT INTO managed_user(name,subject_id,profile_path,phase) VALUES('alice.name.dhttp.net',?1,?2,'pending')",
        rusqlite::params![vec![b'a'; 64],home.join("alice.name").to_string_lossy()],
    )
    .unwrap();
    users::recover(&home, &mut db).unwrap();
    users::recover(&home, &mut db).unwrap();
    assert_eq!(
        std::fs::read_to_string(home.join("alice.name/marker")).unwrap(),
        "original"
    );
    db.execute("UPDATE managed_user SET phase='deleting'", [])
        .unwrap();
    std::fs::create_dir_all(home.join(".system/trash")).unwrap();
    std::fs::rename(
        home.join("alice.name"),
        home.join(".system/trash/alice.name"),
    )
    .unwrap();
    users::recover(&home, &mut db).unwrap();
    users::recover(&home, &mut db).unwrap();
    assert!(!home.join(".system/trash/alice.name").exists());
    assert_eq!(
        db.query_row("SELECT count(*) FROM managed_user", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    std::fs::create_dir_all(home.join("alice.name")).unwrap();
    std::fs::write(home.join("alice.name/replacement"), "keep").unwrap();
    db.execute(
        "INSERT INTO managed_user(name,subject_id,profile_path,phase) VALUES('alice.name.dhttp.net',?1,?2,'removed')",
        rusqlite::params![vec![b'a'; 64],home.join("alice.name").to_string_lossy()],
    )
    .unwrap();
    users::recover(&home, &mut db).unwrap();
    assert!(home.join("alice.name/replacement").exists());
}

#[test]
fn corrupted_system_database_is_preserved() {
    let root = tempfile::tempdir().unwrap();
    let home = DhttpHome::new(root.path().into());
    std::fs::create_dir(home.join(".system")).unwrap();
    std::fs::write(home.join(".system/database"), b"invalid sqlite").unwrap();
    assert!(open(&home).is_err());
    assert_eq!(
        std::fs::read(home.join(".system/database")).unwrap(),
        b"invalid sqlite"
    );
    assert!(crate::routes::reserved("/sys"));
    assert!(crate::routes::reserved("/sys/user"));
    assert!(!crate::routes::reserved("/system"));
}

#[tokio::test]
async fn administrator_membership_survives_credential_directory_deletion() {
    let (_root, home, database) = database();
    for name in ["admin.name", "client.name"] {
        let profile = users::profile(&home, &storage::canonical(name).unwrap());
        std::fs::create_dir_all(profile.ssl_dir()).unwrap();
        let endpoint = crate::test_support::identity::endpoint(name);
        let local = endpoint.local_authority().unwrap();
        let pem = local
            .certificates()
            .iter()
            .map(|cert| {
                format!(
                    "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
                    base64::engine::general_purpose::STANDARD.encode(cert.as_ref())
                )
            })
            .collect::<String>();
        std::fs::write(profile.cert_path(), pem).unwrap();
    }
    storage::put_admin(&*database.lock().await, "admin.name.dhttp.net", &[b'a'; 64]).unwrap();
    let count: i64 = database
        .lock()
        .await
        .query_row("SELECT count(*) FROM administrator", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);
    std::fs::remove_dir_all(home.join("admin.name")).unwrap();
    let reopened = open(&home).unwrap();
    assert!(storage::administrator(&*reopened.lock().await, &visitor("admin.name", b'a')).unwrap());
    let mut db = database.lock().await;
    let deleted = users::delete(&home, &mut db, "client.name").await.unwrap();
    assert_eq!(deleted["deleted"], true);
    assert!(!home.join("client.name").exists());
}

#[tokio::test]
async fn native_system_api_uses_only_builtin_routes() {
    let (_root, home, database) = database();
    let endpoint = crate::test_support::identity::endpoint("device.name");
    let authority = endpoint.local_authority().unwrap();
    let owner = identity(authority.name(), authority.certificates()).unwrap();
    storage::put_admin(
        &*database.lock().await,
        owner.name(),
        owner.subject_id().as_bytes(),
    )
    .unwrap();
    std::fs::create_dir_all(home.join("lib/sys")).unwrap();
    std::fs::write(home.join("lib/sys/version.wasm"), b"invalid component").unwrap();
    std::fs::write(home.join("lib/sys/extra.wasm"), b"invalid component").unwrap();
    let (commands, _receiver) = mpsc::channel(8);
    let app =
        router(home.clone(), database.clone(), commands).layer(axum::Extension(endpoint.clone()));
    for (path, status) in [
        ("/sys/version", StatusCode::OK),
        ("/sys/info", StatusCode::OK),
        ("/sys/admin", StatusCode::OK),
        ("/sys/user", StatusCode::OK),
        ("/sys/extra", StatusCode::NOT_FOUND),
    ] {
        let response = app
            .clone()
            .oneshot(request(&endpoint, Method::GET, path, Body::empty(), false))
            .await
            .unwrap();
        assert_eq!(response.status(), status, "{path}");
    }
    assert_eq!(
        database
            .lock()
            .await
            .query_row("SELECT count(*) FROM administrator", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn first_enabled_host_is_registered_once_and_revocation_survives_reopening() {
    let (_root, home, database) = database();
    let first = crate::test_support::identity::endpoint("first.name");
    let second = crate::test_support::identity::endpoint("second.name");
    register_host(&database, &users::profile(&home, first.name()), &first)
        .await
        .unwrap();
    register_host(&database, &users::profile(&home, second.name()), &second)
        .await
        .unwrap();
    {
        let db = database.lock().await;
        let first_owner = identity(
            first.name(),
            first.local_authority().unwrap().certificates(),
        )
        .unwrap();
        let second_owner = identity(
            second.name(),
            second.local_authority().unwrap().certificates(),
        )
        .unwrap();
        assert!(storage::administrator(&db, &first_owner).unwrap());
        assert!(!storage::administrator(&db, &second_owner).unwrap());
        assert_eq!(
            db.query_row("SELECT first_administrator FROM device", [], |row| row
                .get::<_, String>(
                0
            ))
            .unwrap(),
            first.name()
        );
        db.execute("DELETE FROM administrator", []).unwrap();
    }
    let reopened = open(&home).unwrap();
    register_host(&reopened, &users::profile(&home, first.name()), &first)
        .await
        .unwrap();
    register_host(&reopened, &users::profile(&home, second.name()), &second)
        .await
        .unwrap();
    assert_eq!(
        reopened
            .lock()
            .await
            .query_row("SELECT count(*) FROM administrator", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn first_host_registration_preserves_existing_administrators() {
    let (_root, home, database) = database();
    storage::put_admin(
        &*database.lock().await,
        "configured.name.dhttp.net",
        &[b'a'; 64],
    )
    .unwrap();
    let candidate = crate::test_support::identity::endpoint("ordinary.name");
    register_host(
        &database,
        &users::profile(&home, candidate.name()),
        &candidate,
    )
    .await
    .unwrap();
    let db = database.lock().await;
    assert_eq!(
        db.query_row("SELECT first_administrator FROM device", [], |row| row
            .get::<_, String>(
            0
        ))
        .unwrap(),
        "configured.name.dhttp.net"
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM administrator", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn administrator_api_uses_only_name_and_subject_id() {
    let (_root, home, database) = database();
    let endpoint = crate::test_support::identity::endpoint("owner.name");
    register_host(
        &database,
        &users::profile(&home, endpoint.name()),
        &endpoint,
    )
    .await
    .unwrap();
    let (commands, _receiver) = mpsc::channel(8);
    let app = router(home, database.clone(), commands).layer(axum::Extension(endpoint.clone()));
    let body = json!({"name":"member.name","subject_id":"b".repeat(64)});
    for field in ["kind", "location", "local_path"] {
        let mut invalid = body.clone();
        invalid[field] = "unused".into();
        let response = app
            .clone()
            .oneshot(request(
                &endpoint,
                Method::POST,
                "/sys/admin",
                Body::from(invalid.to_string()),
                false,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{field}");
    }
    for name in ["member.name", "member.name~", "member.name.dhttp.net"] {
        let mut body = body.clone();
        body["name"] = name.into();
        let response = app
            .clone()
            .oneshot(request(
                &endpoint,
                Method::POST,
                "/sys/admin",
                Body::from(body.to_string()),
                false,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    let response = app
        .oneshot(request(
            &endpoint,
            Method::GET,
            "/sys/admin",
            Body::empty(),
            false,
        ))
        .await
        .unwrap();
    let administrators: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(administrators.as_array().unwrap().len(), 2);
    for administrator in administrators.as_array().unwrap() {
        let fields = administrator.as_object().unwrap();
        assert_eq!(fields.len(), 2);
        assert!(fields.contains_key("name"));
        assert!(fields.contains_key("subject_id"));
    }
}

#[tokio::test]
async fn administrator_schema_contains_only_identity_bindings() {
    let (_root, _home, database) = database();
    let db = database.lock().await;
    let fields = db
        .prepare("PRAGMA table_info(administrator)")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(fields, ["name", "subject_id"]);
}

#[tokio::test]
async fn database_upgrade_preserves_all_administrators_and_first_registration() {
    let root = tempfile::tempdir().unwrap();
    let home = DhttpHome::new(root.path().into());
    std::fs::create_dir(home.join(".system")).unwrap();
    let db = Connection::open(home.join(".system/database")).unwrap();
    db.execute_batch(include_str!("../../fixtures/system/v1/schema.sql"))
        .unwrap();
    db.execute("INSERT INTO administrator VALUES('first.name.dhttp.net',?1,'/device/first.name','regular')",[vec![b'a';64]]).unwrap();
    db.execute(
        "INSERT INTO administrator VALUES('second.name.dhttp.net',?1,NULL,'auxiliary')",
        [vec![b'b'; 64]],
    )
    .unwrap();
    db.execute("INSERT INTO device VALUES(1,'first.name.dhttp.net')", [])
        .unwrap();
    db.execute(
        "INSERT INTO managed_user VALUES('child.name.dhttp.net',?1,'/device/child.name','pending')",
        [vec![b'c'; 64]],
    )
    .unwrap();
    drop(db);
    let database = open(&home).unwrap();
    let db = database.lock().await;
    assert!(storage::administrator(&db, &visitor("first.name", b'a')).unwrap());
    assert!(storage::administrator(&db, &visitor("second.name", b'b')).unwrap());
    assert_eq!(
        db.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        db.query_row("SELECT first_administrator FROM device", [], |row| row
            .get::<_, String>(
            0
        ))
        .unwrap(),
        "first.name.dhttp.net"
    );
    assert_eq!(
        db.query_row("SELECT phase FROM managed_user", [], |row| row
            .get::<_, String>(0))
            .unwrap(),
        "pending"
    );
    storage::put_admin(&db, "third.name.dhttp.net", &[b'c'; 64]).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM administrator", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        3
    );
}
