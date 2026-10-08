use std::path::Path;

use dhttp_home::identity::IdentityProfile;
use rusqlite::Connection;

use super::*;

fn profile(root: &Path) -> IdentityProfile {
    let profile = IdentityProfile::try_from(root.join("alice")).unwrap();
    std::fs::create_dir_all(profile.db_dir()).unwrap();
    let db = Connection::open(profile.config_db_path()).unwrap();
    db.execute_batch("PRAGMA user_version=1; CREATE TABLE settings(listen INTEGER); INSERT INTO settings VALUES(1); CREATE TABLE proxy_locations(location TEXT,proxy_pass TEXT);").unwrap();
    profile
}

#[test]
fn config_api_reads_do_not_initialize_deleted_or_empty_databases() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    std::fs::remove_file(profile.config_db_path()).unwrap();
    assert!(config_database(&profile, true, None).is_err());
    assert!(!profile.config_db_path().exists());
    std::fs::write(profile.config_db_path(), []).unwrap();
    assert!(config_database(&profile, true, None).is_err());
    assert_eq!(profile.config_db_path().metadata().unwrap().len(), 0);
}
#[test]
fn config_requires_one_row_and_local_http_upstreams() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    assert_eq!(load_server_config(&profile).unwrap().listen, 1);
    let db = Connection::open(profile.config_db_path()).unwrap();
    db.execute(
        "INSERT INTO proxy_locations VALUES('/plain/','http://127.0.0.1:8080')",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO proxy_locations VALUES('/replace/','http://127.0.0.1:8080/')",
        [],
    )
    .unwrap();
    let config = load_server_config(&profile).unwrap();
    assert!(
        config.proxy_locations[0]
            .proxy_pass
            .path_and_query
            .is_none()
    );
    assert!(
        config.proxy_locations[1]
            .proxy_pass
            .path_and_query
            .is_some()
    );
    db.execute(
        "INSERT INTO proxy_locations VALUES('/shorthand','127.0.0.1:8080')",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO proxy_locations VALUES('/ipv6','http://[::1]:8081/')",
        [],
    )
    .unwrap();
    let expanded = load_server_config(&profile).unwrap();
    assert_eq!(
        expanded
            .proxy_locations
            .iter()
            .find(|p| p.location == "/shorthand")
            .unwrap()
            .proxy_pass
            .authority
            .as_ref()
            .unwrap(),
        "127.0.0.1:8080"
    );
    assert!(
        expanded
            .proxy_locations
            .iter()
            .find(|p| p.location == "/shorthand")
            .unwrap()
            .proxy_pass
            .path_and_query
            .is_none()
    );
    assert_eq!(
        expanded
            .proxy_locations
            .iter()
            .find(|p| p.location == "/ipv6")
            .unwrap()
            .proxy_pass
            .authority
            .as_ref()
            .unwrap(),
        "[::1]:8081"
    );
    db.execute(
        "INSERT INTO proxy_locations VALUES('/bad','https://bob.dhttp.net')",
        [],
    )
    .unwrap();
    assert!(load_server_config(&profile).is_err());
    db.execute("DELETE FROM proxy_locations WHERE location='/bad'", [])
        .unwrap();
    for target in [
        "http://example.com:8080",
        "http://192.168.1.2:8080",
        "https://127.0.0.1:8080",
    ] {
        db.execute("INSERT INTO proxy_locations VALUES('/bad',?1)", [target])
            .unwrap();
        assert!(load_server_config(&profile).is_err(), "{target}");
        db.execute("DELETE FROM proxy_locations WHERE location='/bad'", [])
            .unwrap();
    }
    db.execute("DELETE FROM proxy_locations", []).unwrap();
    db.execute("INSERT INTO settings VALUES(1)", []).unwrap();
    assert!(load_server_config(&profile).is_err());
}
#[test]
fn file_namespace_cannot_be_proxied() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    let db = Connection::open(profile.config_db_path()).unwrap();
    for location in ["/file", "/file/", "= /file/a"] {
        db.execute("DELETE FROM proxy_locations", []).unwrap();
        db.execute(
            "INSERT INTO proxy_locations VALUES(?1,'127.0.0.1:8080')",
            [location],
        )
        .unwrap();
        assert!(matches!(
            load_server_config(&profile),
            Err(Error::InvalidConfig(_))
        ));
    }
}

#[test]
fn config_api_updates_resources_independently_and_preserves_upstream_paths() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    let db = Connection::open(profile.config_db_path()).unwrap();
    db.execute_batch("ALTER TABLE settings ADD COLUMN exec INTEGER NOT NULL DEFAULT 1;")
        .unwrap();
    let proxies = json!([
        {"location":"/plain", "proxy_pass":"127.0.0.1:8080"},
        {"location":"/replace", "proxy_pass":"http://127.0.0.1:8080/"}
    ]);
    let saved = config_database(&profile, false, Some(proxies)).unwrap();
    assert_eq!(saved[0]["proxy_pass"], "http://127.0.0.1:8080");
    assert_eq!(saved[1]["proxy_pass"], "http://127.0.0.1:8080/");
    assert_eq!(
        config_database(&profile, true, None).unwrap(),
        json!({"listen":1})
    );
    assert_eq!(
        config_database(&profile, true, Some(json!({"listen":3}))).unwrap(),
        json!({"listen":3})
    );
    assert_eq!(config_database(&profile, false, None).unwrap(), saved);
    assert_eq!(
        db.query_row("SELECT exec FROM settings", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        1
    );
    config_database(&profile, false, Some(json!([]))).unwrap();
    assert!(
        load_server_config(&profile)
            .unwrap()
            .proxy_locations
            .is_empty()
    );
}

#[test]
fn invalid_config_api_input_never_changes_saved_resources() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    let before = json!([{ "location":"/old", "proxy_pass":"http://127.0.0.1:8080" }]);
    config_database(&profile, false, Some(before.clone())).unwrap();
    for payload in [
        json!({}),
        json!(null),
        json!([]),
        json!({"exec":true}),
        json!({"listen":1.0}),
        json!({"listen":-1}),
        json!({"listen":4}),
        json!({"exec":null}),
        json!({"listen":2,"extra":true}),
    ] {
        assert!(matches!(
            config_database(&profile, true, Some(payload)),
            Err(Error::BadRequest(_))
        ));
        assert_eq!(
            config_database(&profile, true, None).unwrap(),
            json!({"listen":1})
        );
    }
    for payload in [
        json!({"proxy_locations": []}),
        json!([{"location":"/a","proxy_pass":"127.0.0.1:8080","extra":1}]),
        json!([{"location":"/a","proxy_pass":null}]),
        json!([{"location":"/a","proxy_pass":"127.0.0.1:8080"},{"location":"/a","proxy_pass":"127.0.0.1:8081"}]),
        json!([{"location":"/valid","proxy_pass":"127.0.0.1:8080"},{"location":"/sys/settings","proxy_pass":"127.0.0.1:8081"}]),
        json!([{"location":"/sys","proxy_pass":"127.0.0.1:8080"}]),
        json!([{"location":"/bad","proxy_pass":"http://192.168.1.1:8080"}]),
        json!([{"location":"/bad","proxy_pass":"http://127.0.0.1:0"}]),
        json!([{"location":"/bad","proxy_pass":"https://127.0.0.1:8080"}]),
    ] {
        assert!(matches!(
            config_database(&profile, false, Some(payload)),
            Err(Error::BadRequest(_))
        ));
        assert_eq!(config_database(&profile, false, None).unwrap(), before);
    }
    // /sys uses a path-segment boundary, so unrelated /system proxies are valid.
    assert!(
        parse_proxy_json(&json!([{"location":"/system","proxy_pass":"127.0.0.1:8080"}])).is_ok()
    );
}

#[test]
fn proxy_replacement_rolls_back_a_database_failure_after_an_insert() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    let before = json!([{ "location":"/old", "proxy_pass":"http://127.0.0.1:8080" }]);
    config_database(&profile, false, Some(before.clone())).unwrap();
    let conn = Connection::open(profile.config_db_path()).unwrap();
    conn.execute_batch("CREATE TRIGGER reject_proxy BEFORE INSERT ON proxy_locations WHEN NEW.location='/z' BEGIN SELECT RAISE(ABORT,'test failure'); END;").unwrap();
    let result = config_database(
        &profile,
        false,
        Some(json!([
            {"location":"/a","proxy_pass":"127.0.0.1:8080"},
            {"location":"/z","proxy_pass":"127.0.0.1:8081"}
        ])),
    );
    assert!(matches!(result, Err(Error::ConfigDatabase(_))));
    assert_eq!(config_database(&profile, false, None).unwrap(), before);
}

fn api_endpoint(name: &str) -> dhttp::Endpoint {
    let mut params = rcgen::CertificateParams::new(vec![name.to_owned()]).unwrap();
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
    dhttp::Endpoint::new(
        qbase::endpoint::Endpoint::new(
            &qtls::default_provider(),
            name,
            vec![cert.der().clone()],
            qtls::PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
            vec![1],
        )
        .unwrap(),
    )
}

fn api_request(method: Method, path: &str, body: Body) -> Request<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .unwrap();
    request.extensions_mut().insert(Visitor::new(
        "alice.dhttp.net",
        SubjectId::new("a".repeat(64).as_bytes()).unwrap(),
    ));
    request
}

#[tokio::test]
async fn config_routes_enforce_protocol_and_owner_requirements() {
    use tower::ServiceExt;
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    let app = config_router(profile.clone(), api_endpoint("alice.dhttp.net"));
    let response = app
        .clone()
        .oneshot(api_request(Method::GET, "/sys/settings", Body::empty()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(response.headers()["supported-versions"], "v1");
    assert_eq!(
        serde_json::from_slice::<Value>(
            &axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap()
        )
        .unwrap(),
        json!({"listen":1})
    );
    for (versions, expected) in [
        ("v2, v1", StatusCode::OK),
        ("v2", StatusCode::HTTP_VERSION_NOT_SUPPORTED),
    ] {
        let mut request = api_request(Method::GET, "/sys/proxies", Body::empty());
        request
            .headers_mut()
            .insert("accept-versions", versions.parse().unwrap());
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            expected
        );
    }
    for (method, path, allow) in [
        (Method::PUT, "/sys/settings", "GET, PATCH"),
        (Method::PATCH, "/sys/proxies", "GET, PUT"),
    ] {
        let response = app
            .clone()
            .oneshot(api_request(method, path, Body::empty()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(response.headers()[header::ALLOW], allow);
    }
    for (body, expected) in [
        (Body::from("{"), StatusCode::BAD_REQUEST),
        (
            Body::from(json!({"exec":null}).to_string()),
            StatusCode::BAD_REQUEST,
        ),
        (
            Body::from(" ".repeat(64 * 1024 + 1)),
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
    ] {
        assert_eq!(
            app.clone()
                .oneshot(api_request(Method::PATCH, "/sys/settings", body))
                .await
                .unwrap()
                .status(),
            expected
        );
    }
    let mut request = api_request(Method::PATCH, "/sys/settings", Body::from("{}"));
    request
        .headers_mut()
        .insert(header::CONTENT_TYPE, "text/plain".parse().unwrap());
    assert_eq!(
        app.clone().oneshot(request).await.unwrap().status(),
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    for visitor in [
        None,
        Some(Visitor::new(
            "bob.dhttp.net",
            SubjectId::new("a".repeat(64).as_bytes()).unwrap(),
        )),
        Some(Visitor::new(
            "alice.dhttp.net",
            SubjectId::new("b".repeat(64).as_bytes()).unwrap(),
        )),
    ] {
        let mut request = api_request(Method::GET, "/sys/settings", Body::empty());
        request.extensions_mut().remove::<Visitor>();
        if let Some(visitor) = visitor {
            request.extensions_mut().insert(visitor);
        }
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        config_database(&profile, true, None).unwrap(),
        json!({"listen":1})
    );
}

#[tokio::test]
async fn config_routes_do_not_trust_forged_visitors_after_daccess_authorization() {
    use std::sync::Arc;

    use tower::ServiceExt;
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    let endpoint = api_endpoint("alice.dhttp.net");
    let access = Arc::new(
        access_control::AccessService::load_from_db(
            "sqlite::memory:",
            endpoint.name(),
            &SubjectId::new("a".repeat(64).as_bytes()).unwrap(),
        )
        .await
        .unwrap(),
    );
    access
        .set_policy(
            access_control::Method::Unspecified,
            "/sys",
            access_control::Effect::Allow,
            access_control::Grantee::Anony,
        )
        .await
        .unwrap();
    let app = config_router(profile, endpoint.clone()).layer(axum::middleware::from_fn_with_state(
        access,
        crate::routes::authorize,
    ));
    let mut request = api_request(Method::GET, "/sys/settings", Body::empty());
    request.extensions_mut().insert(dhttp::HandshakeSummary {
        alpn: None,
        local: Some(endpoint.local_authority().unwrap().clone()),
        remote: None,
    });
    assert_eq!(
        app.oneshot(request).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
}
