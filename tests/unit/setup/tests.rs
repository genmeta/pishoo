use std::path::Path;

use dhttp_home::identity::IdentityProfile;
use rusqlite::Connection;

use super::*;

fn profile(root: &Path) -> IdentityProfile {
    let profile = IdentityProfile::try_from(root.join("alice")).unwrap();
    std::fs::create_dir_all(profile.join(".volume/sys/setting")).unwrap();
    load_server_config(&profile).unwrap();
    let db = Connection::open(profile.join(".volume/sys/setting/database")).unwrap();
    db.execute("INSERT INTO setting VALUES('listen','internal')", [])
        .unwrap();
    profile
}

#[test]
fn config_api_reads_do_not_initialize_deleted_or_empty_databases() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    std::fs::remove_file(profile.join(".volume/sys/setting/database")).unwrap();
    assert!(
        config_database(
            &profile,
            &Method::GET,
            &http::Uri::from_static("/std/sys/listen"),
            None
        )
        .is_err()
    );
    assert!(!profile.join(".volume/sys/setting/database").exists());
    std::fs::write(profile.join(".volume/sys/setting/database"), []).unwrap();
    assert!(
        config_database(
            &profile,
            &Method::GET,
            &http::Uri::from_static("/std/sys/listen"),
            None
        )
        .is_err()
    );
    assert_eq!(
        profile
            .join(".volume/sys/setting/database")
            .metadata()
            .unwrap()
            .len(),
        0
    );
}
#[test]
fn persistent_config_connection_reads_external_commits_and_ends_each_transaction() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    let mut reader = open_config(&profile, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let writer = Connection::open(profile.join(".volume/sys/setting/database")).unwrap();
    assert_eq!(read_server_config(&mut reader).unwrap().listen, "internal");
    assert!(reader.is_autocommit());
    writer
        .execute("UPDATE setting SET value='external' WHERE key='listen'", [])
        .unwrap();
    assert_eq!(read_server_config(&mut reader).unwrap().listen, "external");
    assert!(reader.is_autocommit());
    writer
        .execute("UPDATE setting SET value='invalid' WHERE key='listen'", [])
        .unwrap();
    assert!(read_server_config(&mut reader).is_err());
    assert!(reader.is_autocommit());
    writer
        .execute("UPDATE setting SET value='all' WHERE key='listen'", [])
        .unwrap();
    assert_eq!(read_server_config(&mut reader).unwrap().listen, "all");
    assert!(reader.is_autocommit());
    // Routine reads preserve the current schema.
    assert_eq!(
        writer
            .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        3
    );
}

#[test]
fn config_requires_one_row_and_local_http_upstreams() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    assert_eq!(load_server_config(&profile).unwrap().listen, "internal");
    let db = Connection::open(profile.join(".volume/sys/setting/database")).unwrap();
    db.execute(
        "INSERT INTO proxy_location VALUES('/plain/','http://127.0.0.1:8080')",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO proxy_location VALUES('/replace/','http://127.0.0.1:8080/')",
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
        "INSERT INTO proxy_location VALUES('/shorthand','127.0.0.1:8080')",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO proxy_location VALUES('/ipv6','http://[::1]:8081/')",
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
        "INSERT INTO proxy_location VALUES('/bad','https://bob.dhttp.net')",
        [],
    )
    .unwrap();
    assert!(load_server_config(&profile).is_err());
    db.execute("DELETE FROM proxy_location WHERE location='/bad'", [])
        .unwrap();
    for target in [
        "http://example.com:8080",
        "http://192.168.1.2:8080",
        "https://127.0.0.1:8080",
    ] {
        db.execute("INSERT INTO proxy_location VALUES('/bad',?1)", [target])
            .unwrap();
        assert!(load_server_config(&profile).is_err(), "{target}");
        db.execute("DELETE FROM proxy_location WHERE location='/bad'", [])
            .unwrap();
    }
    db.execute("DELETE FROM proxy_location", []).unwrap();
    assert!(
        db.execute(
            "INSERT INTO setting(key,value) VALUES('listen','internal')",
            []
        )
        .is_err()
    );
}
#[test]
fn only_std_namespace_is_reserved_for_proxy_locations() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    let db = Connection::open(profile.join(".volume/sys/setting/database")).unwrap();
    for location in [
        "/file",
        "/file/",
        "= /file/a",
        "/contact",
        "/contacts",
        "/acl",
        "/app/workspace",
        "/workspace-api",
        "/chat-api",
        "/pishoo",
        "/.pishoo",
        "/std-extra",
        "/standard",
    ] {
        db.execute("DELETE FROM proxy_location", []).unwrap();
        db.execute(
            "INSERT INTO proxy_location VALUES(?1,'127.0.0.1:8080')",
            [location],
        )
        .unwrap();
        assert!(load_server_config(&profile).is_ok(), "{location}");
    }
    for location in [
        "/std",
        "/std/",
        "/std/file",
        "= /std/file/a",
        "/std/api/note",
        "/std/unknown/",
    ] {
        db.execute("DELETE FROM proxy_location", []).unwrap();
        db.execute(
            "INSERT INTO proxy_location VALUES(?1,'127.0.0.1:8080')",
            [location],
        )
        .unwrap();
        assert!(load_server_config(&profile).is_err(), "{location}");
        assert!(
            parse_proxy_json(&json!([{"location":location,"proxy_pass":"127.0.0.1:8080"}]))
                .is_err(),
            "{location}"
        );
        let query = form_urlencoded::Serializer::new(String::new())
            .append_pair("location", location)
            .finish();
        for method in [Method::GET, Method::DELETE] {
            assert!(
                super::config_database(
                    &profile,
                    &method,
                    &format!("/std/sys/forward?{query}").parse().unwrap(),
                    None
                )
                .is_err(),
                "{location}"
            );
        }
    }
}

#[test]
fn ordinary_api_proxy_locations_are_independent_of_loaded_libs() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    let db = Connection::open(profile.join(".volume/sys/setting/database")).unwrap();
    for location in [
        "/api",
        "/api/",
        "/api/websocket",
        "= /api/websocket",
        "/api/note/",
    ] {
        db.execute("DELETE FROM proxy_location", []).unwrap();
        db.execute(
            "INSERT INTO proxy_location VALUES(?1,'127.0.0.1:8080')",
            [location],
        )
        .unwrap();
        assert!(load_server_config(&profile).is_ok(), "{location}");
    }
}

#[test]
fn mount_resource_preserves_listening_and_requires_exact_deletion() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    let uri = http::Uri::from_static("/std/sys/mount");
    let listen = http::Uri::from_static("/std/sys/listen");
    for location in ["/plain", "/replace"] {
        config_database(&profile, &Method::PATCH, &uri,
            Some(json!({"location":location,"proxy_pass":if location == "/plain" {"127.0.0.1:8080"} else {"http://127.0.0.1:8080/"}}))).unwrap();
    }
    let saved = config_database(&profile, &Method::GET, &uri, None).unwrap();
    assert_eq!(saved[0]["proxy_pass"], "http://127.0.0.1:8080");
    assert_eq!(saved[1]["proxy_pass"], "http://127.0.0.1:8080/");
    assert_eq!(
        config_database(
            &profile,
            &Method::PATCH,
            &listen,
            Some(json!({"listen":"all"}))
        )
        .unwrap(),
        json!({"listen":"all"})
    );
    assert_eq!(
        config_database(&profile, &Method::GET, &uri, None).unwrap(),
        saved
    );
    let mismatch = "/std/sys/mount?location=%2Fplain&proxy_pass=http%3A%2F%2F127.0.0.1%3A9090"
        .parse()
        .unwrap();
    assert!(config_database(&profile, &Method::DELETE, &mismatch, None).is_err());
    assert_eq!(
        config_database(&profile, &Method::GET, &uri, None).unwrap(),
        saved
    );
    let deletion = "/std/sys/mount?location=%2Fplain&proxy_pass=127.0.0.1%3A8080"
        .parse()
        .unwrap();
    for _ in 0..2 {
        assert!(
            config_database(&profile, &Method::DELETE, &deletion, None)
                .unwrap()
                .is_null()
        );
    }
    config_database(
        &profile,
        &Method::PATCH,
        &listen,
        Some(json!({"listen":"off"})),
    )
    .unwrap();
    assert_eq!(
        config_database(&profile, &Method::GET, &listen, None).unwrap(),
        json!({"listen":"off"})
    );
    assert_eq!(
        config_database(&profile, &Method::GET, &uri, None)
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    config_database(
        &profile,
        &Method::PATCH,
        &uri,
        Some(json!({"location":"/new","proxy_pass":"127.0.0.1:8082"})),
    )
    .unwrap();
    assert_eq!(load_server_config(&profile).unwrap().listen, "off");
}

#[test]
fn listen_and_mount_reject_each_others_fields_without_mutation() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    let listen = http::Uri::from_static("/std/sys/listen");
    let mount = http::Uri::from_static("/std/sys/mount");
    let before = config_database(
        &profile,
        &Method::PATCH,
        &mount,
        Some(json!({"location":"/old","proxy_pass":"127.0.0.1:8080"})),
    )
    .unwrap();
    for payload in [
        json!({}),
        json!(null),
        json!([]),
        json!({"exec":true}),
        json!({"listen":1}),
        json!({"listen":"auto"}),
        json!({"listen":"loopback"}),
        json!({"listen":"both"}),
        json!({"listen":"ALL"}),
        json!({"listen":"all","enabled":false}),
        json!({"listen":"all","location":"/a","proxy_pass":"127.0.0.1:8080"}),
        json!({"location":"/a","proxy_pass":"127.0.0.1:8080"}),
    ] {
        assert!(matches!(
            config_database(&profile, &Method::PATCH, &listen, Some(payload)),
            Err(Error::BadRequest(_))
        ));
    }
    for payload in [
        json!({"listen":"internal"}),
        json!({"location":"/a","proxy_pass":null}),
        json!({"location":"/a","proxy_pass":"127.0.0.1:8080","extra":1}),
        json!({"location":"/std","proxy_pass":"127.0.0.1:8080"}),
        json!({"location":"/a","proxy_pass":"http://192.168.1.1:8080"}),
        json!({"location":"/a","proxy_pass":"https://127.0.0.1:8080"}),
    ] {
        assert!(matches!(
            config_database(&profile, &Method::PATCH, &mount, Some(payload)),
            Err(Error::BadRequest(_))
        ));
    }
    for target in [
        "/std/sys/mount",
        "/std/sys/mount?location=%2Fold",
        "/std/sys/mount?proxy_pass=127.0.0.1%3A8080",
        "/std/sys/mount?extra=x",
        "/std/sys/mount?location=%2Fold&location=%2Fa&proxy_pass=127.0.0.1%3A8080",
        "/std/sys/listen?location=%2Fold&proxy_pass=127.0.0.1%3A8080",
    ] {
        assert!(matches!(
            config_database(&profile, &Method::DELETE, &target.parse().unwrap(), None),
            Err(Error::BadRequest(_))
        ));
    }
    assert_eq!(
        config_database(&profile, &Method::GET, &mount, None).unwrap(),
        before
    );
    assert_eq!(
        config_database(&profile, &Method::GET, &listen, None).unwrap(),
        json!({"listen":"internal"})
    );
}

#[test]
fn failed_proxy_update_rolls_back_the_previous_rule() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    let uri = http::Uri::from_static("/std/sys/mount");
    let before = config_database(
        &profile,
        &Method::PATCH,
        &uri,
        Some(json!({"location":"/old","proxy_pass":"127.0.0.1:8080"})),
    )
    .unwrap();
    let conn = Connection::open(profile.join(".volume/sys/setting/database")).unwrap();
    conn.execute_batch("CREATE TRIGGER reject_proxy BEFORE INSERT ON proxy_location WHEN NEW.location='/old' BEGIN SELECT RAISE(ABORT,'test failure'); END;").unwrap();
    assert!(matches!(
        config_database(
            &profile,
            &Method::PATCH,
            &uri,
            Some(json!({"location":"/old","proxy_pass":"127.0.0.1:8081"}))
        ),
        Err(Error::ConfigDatabase(_))
    ));
    assert_eq!(
        config_database(&profile, &Method::GET, &uri, None).unwrap(),
        before
    );
}

#[test]
fn concurrent_proxy_updates_preserve_both_rules() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    std::thread::scope(|scope| {
        for location in ["/one", "/two"] {
            let profile = &profile;
            scope.spawn(move || {
                config_database(
                    profile,
                    &Method::PATCH,
                    &http::Uri::from_static("/std/sys/mount"),
                    Some(json!({"location":location,"proxy_pass":"127.0.0.1:8080"})),
                )
                .unwrap()
            });
        }
    });
    assert_eq!(
        config_database(
            &profile,
            &Method::GET,
            &http::Uri::from_static("/std/sys/mount"),
            None
        )
        .unwrap()
        .as_array()
        .unwrap()
        .len(),
        2
    );
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
async fn mount_http_updates_and_deletes_without_changing_listening() {
    use tower::ServiceExt;
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    let app = config_router(profile.clone(), api_endpoint("alice.dhttp.net"));
    let response = app
        .clone()
        .oneshot(api_request(
            Method::PATCH,
            "/std/sys/mount",
            Body::from(json!({"location":"/service","proxy_pass":"127.0.0.1:8080"}).to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let saved: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        saved,
        json!([{"location":"/service","proxy_pass":"http://127.0.0.1:8080"}])
    );
    let mut request = api_request(Method::GET, "/std/sys/mount", Body::empty());
    request.extensions_mut().remove::<Visitor>();
    assert_eq!(
        app.clone().oneshot(request).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    for (uri, status) in [
        (
            "/std/sys/mount?location=%2Fservice&proxy_pass=127.0.0.1%3A9090",
            StatusCode::CONFLICT,
        ),
        (
            "/std/sys/mount?location=%2Fservice&proxy_pass=127.0.0.1%3A8080",
            StatusCode::NO_CONTENT,
        ),
    ] {
        assert_eq!(
            app.clone()
                .oneshot(api_request(Method::DELETE, uri, Body::empty()))
                .await
                .unwrap()
                .status(),
            status
        );
    }
    assert_eq!(load_server_config(&profile).unwrap().listen, "internal");
    assert!(
        load_server_config(&profile)
            .unwrap()
            .proxy_locations
            .is_empty()
    );
}

#[tokio::test]
async fn config_routes_enforce_protocol_and_owner_requirements() {
    use tower::ServiceExt;
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    let app = config_router(profile.clone(), api_endpoint("alice.dhttp.net"));
    let response = app
        .clone()
        .oneshot(api_request(Method::GET, "/std/sys/listen", Body::empty()))
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
        json!({"listen":"internal"})
    );
    for path in ["/std/sys/settings", "/std/sys/proxies"] {
        assert_eq!(
            app.clone()
                .oneshot(api_request(Method::GET, path, Body::empty()))
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
    }
    for (versions, expected) in [
        ("v2, v1", StatusCode::OK),
        ("v2", StatusCode::HTTP_VERSION_NOT_SUPPORTED),
    ] {
        let mut request = api_request(Method::GET, "/std/sys/listen", Body::empty());
        request
            .headers_mut()
            .insert("accept-versions", versions.parse().unwrap());
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            expected
        );
    }
    for (method, path, allow) in [
        (Method::PUT, "/std/sys/listen", "GET, PATCH, DELETE"),
        (Method::POST, "/std/sys/listen", "GET, PATCH, DELETE"),
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
                .oneshot(api_request(Method::PATCH, "/std/sys/listen", body))
                .await
                .unwrap()
                .status(),
            expected
        );
    }
    let mut request = api_request(Method::PATCH, "/std/sys/listen", Body::from("{}"));
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
        let mut request = api_request(Method::GET, "/std/sys/listen", Body::empty());
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
        config_database(
            &profile,
            &Method::GET,
            &http::Uri::from_static("/std/sys/listen"),
            None
        )
        .unwrap(),
        json!({"listen":"internal"})
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
            "/std/pishoo",
            access_control::Effect::Allow,
            access_control::Grantee::Anony,
        )
        .await
        .unwrap();
    let app = config_router(profile, endpoint.clone()).layer(axum::middleware::from_fn_with_state(
        access,
        crate::routes::authorize,
    ));
    let mut request = api_request(Method::GET, "/std/sys/listen", Body::empty());
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

#[tokio::test]
async fn listen_routes_reject_invalid_queries_bodies_and_removed_forward_route() {
    use tower::ServiceExt;
    let root = tempfile::tempdir().unwrap();
    let app = config_router(profile(root.path()), api_endpoint("alice.dhttp.net"));
    for (method, uri, body, status) in [
        (Method::GET, "/std/sys/listen", "x", StatusCode::BAD_REQUEST),
        (
            Method::GET,
            "/std/sys/listen?extra=x",
            "",
            StatusCode::BAD_REQUEST,
        ),
        (
            Method::DELETE,
            "/std/sys/listen?location=%2Fgone",
            "",
            StatusCode::BAD_REQUEST,
        ),
        (
            Method::DELETE,
            "/std/sys/mount?location=%2Fgone&proxy_pass=127.0.0.1%3A8080",
            "",
            StatusCode::NO_CONTENT,
        ),
        (
            Method::DELETE,
            "/std/sys/listen",
            "x",
            StatusCode::BAD_REQUEST,
        ),
        (
            Method::PATCH,
            "/std/sys/listen?location=%2Ftest",
            "{}",
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let response = app
            .clone()
            .oneshot(api_request(method, uri, Body::from(body)))
            .await
            .unwrap();
        assert_eq!(response.status(), status, "{uri}");
        assert_eq!(response.headers()["supported-versions"], "v1");
    }
    for method in [Method::GET, Method::PUT, Method::PATCH, Method::DELETE] {
        assert_eq!(
            app.clone()
                .oneshot(api_request(method, "/std/sys/forward", Body::empty()))
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
    }
}

#[test]
fn listen_accepts_named_modes_and_preserves_mount_rules() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    let uri = http::Uri::from_static("/std/sys/listen");
    let mount = http::Uri::from_static("/std/sys/mount");
    let before = config_database(
        &profile,
        &Method::PATCH,
        &mount,
        Some(json!({"location":"/keep","proxy_pass":"127.0.0.1:8080"})),
    )
    .unwrap();
    for mode in ["internal", "external", "all", "off"] {
        assert_eq!(
            config_database(&profile, &Method::PATCH, &uri, Some(json!({"listen":mode}))).unwrap(),
            json!({"listen":mode})
        );
        assert_eq!(
            config_database(&profile, &Method::GET, &mount, None).unwrap(),
            before
        );
        assert_eq!(load_server_config(&profile).unwrap().listen, mode);
    }
}

#[test]
fn unknown_versions_are_rejected_without_mutation_or_split_registries() {
    for version in [99] {
        let root = tempfile::tempdir().unwrap();
        let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
        std::fs::create_dir_all(profile.join(".volume/sys/setting")).unwrap();
        let path = profile.join(".volume/sys/setting/database");
        let db = Connection::open(&path).unwrap();
        db.execute_batch(&format!("CREATE TABLE setting(key TEXT PRIMARY KEY,value TEXT); INSERT INTO setting VALUES('listen','all'); PRAGMA user_version={version};")).unwrap();
        drop(db);
        let before = std::fs::read(&path).unwrap();
        let error = load_server_config(&profile).unwrap_err().to_string();
        assert!(error.contains("unsupported configuration"), "{error}");
        assert!(
            config_database(
                &profile,
                &Method::PATCH,
                &"/std/sys/listen".parse().unwrap(),
                Some(json!({"listen":"off"}))
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        for name in ["app", "lib", "pkg", "access", "contact"] {
            assert!(!profile.join(format!(".volume/sys/{name}")).exists());
        }
    }
}

#[test]
fn local_configuration_initializes_v3_and_preserves_data_on_repeat() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    assert!(crate::configure(&profile, None, false).is_err());
    assert!(!profile.path().exists());
    assert_eq!(
        crate::configure(&profile, Some("all"), false).unwrap(),
        json!({"listen":"all"})
    );
    let db = Connection::open(profile.join(".volume/sys/setting/database")).unwrap();
    db.execute_batch("INSERT INTO setting VALUES('theme','dark'); INSERT INTO proxy_location VALUES('/api','http://127.0.0.1:8080');").unwrap();
    assert_eq!(
        crate::configure(&profile, None, true).unwrap(),
        json!({"listen":"all"})
    );
    assert_eq!(
        crate::configure(&profile, Some("off"), false).unwrap(),
        json!({"listen":"off"})
    );
    assert_eq!(
        db.query_row("SELECT value FROM setting WHERE key='theme'", [], |r| {
            r.get::<_, String>(0)
        })
        .unwrap(),
        "dark"
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM proxy_location", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        3
    );
    let mut names = std::fs::read_dir(profile.join(".volume/sys"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(names, ["access", "contact", "pkg", "setting"]);
    let pkg = Connection::open(profile.join(".volume/sys/pkg/database")).unwrap();
    assert_eq!(
        pkg.query_row("SELECT count(*) FROM package", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn current_configuration_ignores_legacy_residue_without_mutating_it() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    crate::configure(&profile, Some("internal"), false).unwrap();
    std::fs::create_dir_all(profile.join("db")).unwrap();
    let legacy = profile.config_db_path();
    std::fs::write(&legacy, b"untouched legacy configuration").unwrap();
    assert_eq!(load_server_config(&profile).unwrap().listen, "internal");
    assert_eq!(
        crate::configure(&profile, None, true).unwrap(),
        json!({"listen":"internal"})
    );
    assert_eq!(
        crate::configure(&profile, Some("all"), false).unwrap(),
        json!({"listen":"all"})
    );
    assert_eq!(
        std::fs::read(&legacy).unwrap(),
        b"untouched legacy configuration"
    );
    assert!(!profile.join(".volume/sys/app").exists());
    assert!(!profile.join(".volume/sys/lib").exists());
}

#[test]
fn legacy_path_and_empty_current_initialize_without_importing_old_data() {
    for empty_new in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
        std::fs::create_dir_all(profile.join("db")).unwrap();
        let legacy = profile.config_db_path();
        std::fs::write(&legacy, b"legacy data").unwrap();
        let current = profile.join(".volume/sys/setting/database");
        if empty_new {
            std::fs::create_dir_all(current.parent().unwrap()).unwrap();
            std::fs::write(&current, b"").unwrap();
        }
        for mode in [None, Some("all")] {
            assert!(crate::configure(&profile, mode, true).is_ok());
        }
        assert_eq!(std::fs::read(&legacy).unwrap(), b"legacy data");
        assert!(current.is_file());
        assert_eq!(
            crate::configure(&profile, None, false).unwrap(),
            json!({"listen":"all"})
        );
        assert!(profile.join(".volume/sys/pkg/database").exists());
    }
}

#[test]
fn old_setting_versions_are_archived_and_replaced_by_fresh_defaults() {
    for version in [1, 2] {
        let root = tempfile::tempdir().unwrap();
        let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
        crate::configure(&profile, Some("all"), false).unwrap();
        let path = profile.join(".volume/sys/setting/database");
        let db = Connection::open(&path).unwrap();
        db.execute_batch(&format!("INSERT INTO proxy_location VALUES('/old','http://127.0.0.1:8080'); PRAGMA user_version={version};")).unwrap();
        drop(db);
        let before = std::fs::read(&path).unwrap();
        let registry = std::fs::read(profile.join(".volume/sys/pkg/database")).unwrap();
        assert_eq!(
            crate::configure(&profile, None, true).unwrap(),
            json!({"listen":"off"})
        );
        let archives = std::fs::read_dir(profile.join(".volume/sys"))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".setting-v")
            })
            .collect::<Vec<_>>();
        assert_eq!(archives.len(), 1);
        assert_eq!(
            std::fs::read(archives[0].path().join("setting/database")).unwrap(),
            before
        );
        assert_eq!(
            std::fs::read(profile.join(".volume/sys/pkg/database")).unwrap(),
            registry
        );
        let db = Connection::open(&path).unwrap();
        assert_eq!(
            db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            3
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM proxy_location", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            crate::configure(&profile, Some("internal"), false).unwrap(),
            json!({"listen":"internal"})
        );
        assert!(!profile.join(".volume/sys/app").exists());
        assert!(!profile.join(".volume/sys/lib").exists());
    }
}
