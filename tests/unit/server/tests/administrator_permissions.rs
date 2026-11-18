use super::*;

#[tokio::test]
async fn administrator_group_visibility_follows_verified_device_membership() {
    let root = tempfile::tempdir().unwrap();
    let mut server = server(root.path()).await;
    let local = server.endpoint.local_authority().unwrap();
    let ski = dhttp_home::certificate::extract_dhttp_subject_key_identifier(local.certificates())
        .unwrap();
    server.access = Arc::new(
        access_control::AccessService::load_from_db(
            "sqlite::memory:",
            local.name(),
            &access_control::SubjectId::new(ski.owner_hash().as_str().as_bytes()).unwrap(),
        )
        .await
        .unwrap(),
    );
    let home = DhttpHome::new(root.path().into());
    let database = crate::system::open(&home).unwrap();
    let (commands, _receiver) = tokio::sync::mpsc::channel(8);
    server.system_router = crate::system::router(home, database.clone(), commands).merge(
        crate::system::permissions_router(
            server.profile.clone(),
            server.access.clone(),
            database.clone(),
        ),
    );
    server
        .access
        .set_policy(
            access_control::Method::Specified(http::Method::GET),
            "/std/acl/permissions",
            access_control::Effect::Allow,
            access_control::Grantee::All,
        )
        .await
        .unwrap();
    let mut config =
        crate::setup::open_config(&server.profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
            .unwrap();
    let tx = config.transaction().unwrap();
    crate::routes::permissions(
        &tx,
        "echo",
        &[(
            "read".into(),
            "Read echo".into(),
            vec![("GET".into(), "/echo/value".into())],
        )],
    )
    .unwrap();
    tx.commit().unwrap();
    let forged = access_control::Visitor::new(
        local.name(),
        access_control::SubjectId::new(ski.owner_hash().as_str().as_bytes()).unwrap(),
    );
    let catalog = |details: bool, anonymous: bool| {
        let app = current_router(&server);
        let local = local.clone();
        let forged = forged.clone();
        async move {
            let mut request = Request::builder()
                .uri(if details {
                    "/std/acl/permissions?details=1"
                } else {
                    "/std/acl/permissions"
                })
                .body(AxumBody::empty())
                .unwrap();
            if anonymous {
                request.extensions_mut().insert(dhttp::HandshakeSummary {
                    alpn: None,
                    local: Some(local),
                    remote: None,
                });
                // Even an injected owner Visitor must not reveal the device group.
                request.extensions_mut().insert(forged);
            } else {
                request.extensions_mut().insert(local);
            }
            let response = app.oneshot(request).await.unwrap();
            assert_eq!(response.status(), http::StatusCode::OK);
            assert_eq!(response.headers()[http::header::CACHE_CONTROL], "no-store");
            serde_json::from_slice::<serde_json::Value>(
                &axum::body::to_bytes(response.into_body(), 65536)
                    .await
                    .unwrap(),
            )
            .unwrap()
        }
    };
    let baseline = catalog(true, false).await;
    assert_eq!(baseline.as_object().unwrap().len(), 1);
    crate::system::register_host(&database, &server.profile, &server.endpoint)
        .await
        .unwrap();
    let id = "pishoo · 系统管理 / System administration";
    let visible = catalog(true, false).await;
    let group = &visible[id];
    assert_eq!(group["management"], "administrator");
    assert_eq!(group["title"]["zh-CN"], "系统管理");
    assert_eq!(group["members"], serde_json::json!([server.name()]));
    assert_eq!(
        group["endpoints"]["/sys/restart"],
        serde_json::json!(["POST"])
    );
    assert!(group["endpoints"].get("/sys").is_none());
    assert_eq!(group["endpoints"].as_object().unwrap().len(), 6);
    for (key, value) in baseline.as_object().unwrap() {
        assert_eq!(&visible[key], value);
    }
    assert_eq!(catalog(false, false).await[id], group["endpoints"]);
    assert_eq!(catalog(true, true).await, baseline);
    database
        .lock()
        .await
        .execute(
            "UPDATE administrator SET subject_id=?1 WHERE name=?2",
            rusqlite::params![b"different owner".as_slice(), server.name()],
        )
        .unwrap();
    assert_eq!(catalog(true, false).await, baseline);
    database
        .lock()
        .await
        .execute(
            "UPDATE administrator SET subject_id=?1 WHERE name=?2",
            rusqlite::params![ski.owner_hash().as_str().as_bytes(), server.name()],
        )
        .unwrap();
    assert!(catalog(true, false).await.get(id).is_some());
    database
        .lock()
        .await
        .execute("DELETE FROM administrator", [])
        .unwrap();
    assert_eq!(catalog(true, false).await, baseline);
}
