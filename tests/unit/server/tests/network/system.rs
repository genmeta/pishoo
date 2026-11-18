//! Native device administration across genuine peer authentication and HTTP/3.
use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "run alone; owns process-global TLS roots and resolver and binds loopback UDP"]
async fn device_admin_registration_and_authorization_over_h3() {
    use http_body_util::BodyExt;

    let root = tempfile::tempdir().unwrap();
    credentials::generate(
        root.path(),
        &[
            ("receiver", "receiver.dhttp.net"),
            ("alice", "alice.dhttp.net"),
            ("bob", "bob.dhttp.net"),
        ],
    );
    let home = DhttpHome::new(root.path().join("device"));
    std::fs::create_dir(home.as_path()).unwrap();
    std::fs::rename(root.path().join("receiver"), home.join("receiver")).unwrap();
    dhttp::DhttpNetwork::init().await.unwrap();
    qtls::RootCerts::set([qtls::CertificateDer::from(
        std::fs::read(root.path().join("ca.der")).unwrap(),
    )])
    .unwrap();
    let peer = Arc::new(qprotocol::UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap());
    let address = EndpointAddr::direct(peer.local_addr().unwrap());
    qprotocol::Dock::global()
        .add(peer.clone())
        .unwrap()
        .unwrap();
    let _socket = scopeguard::guard(peer, |peer| {
        qprotocol::Dock::global().remove(&peer);
    });
    dhttp::resolve::Resolver::add(Arc::new(PeerResolver(address)));
    let database = crate::system::open(&home).unwrap();
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let (commands, _receiver) = tokio::sync::mpsc::channel(8);
    let mut server = Server::load(profile(home.as_path(), "receiver"), runtime)
        .await
        .unwrap();
    server.system_router = crate::system::router(home.clone(), database.clone(), commands).merge(
        crate::system::permissions_router(
            server.profile.clone(),
            server.access.clone(),
            database.clone(),
        ),
    );
    *server.router.write().unwrap() = current_router(&server);
    let app = server.router.clone();
    let listener = server
        .endpoint
        .listen(
            dhttp::Scope::Loopback.into(),
            tower::service_fn(move |request: Request<Body>| {
                let app = app.read().unwrap().clone();
                async move { app.oneshot(request.map(AxumBody::new)).await }
            }),
        )
        .await
        .unwrap();
    let _listener = scopeguard::guard(tokio::spawn(listener), |task| task.abort());
    let alice = dhttp::Endpoint::load_from(root.path().join("alice"))
        .await
        .unwrap();
    let bob = dhttp::Endpoint::load_from(root.path().join("bob"))
        .await
        .unwrap();
    let get = |client: dhttp::Endpoint| async move {
        let response = tokio::time::timeout(
            Duration::from_secs(10),
            client.get("https://receiver.dhttp.net/sys/version".parse().unwrap()),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response.version(), http::Version::HTTP_3);
        let status = response.status();
        response.into_body().collect().await.unwrap();
        status
    };
    assert_eq!(get(alice.clone()).await, StatusCode::FORBIDDEN);
    let catalog = |client: dhttp::Endpoint| async move {
        let response = client
            .get(
                "https://receiver.dhttp.net/std/acl/permissions?details=1"
                    .parse()
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        serde_json::from_slice::<serde_json::Value>(
            &response.into_body().collect().await.unwrap().to_bytes(),
        )
        .unwrap()
    };
    let permission = "pishoo · 系统管理 / System administration";
    assert!(catalog(alice.clone()).await.get(permission).is_none());
    assert!(
        catalog(server.endpoint.clone())
            .await
            .get(permission)
            .is_none()
    );
    assert_eq!(
        database
            .lock()
            .await
            .query_row("SELECT count(*) FROM administrator", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    crate::system::register_host(&database, &server.profile, &server.endpoint)
        .await
        .unwrap();
    assert_eq!(get(server.endpoint.clone()).await, StatusCode::OK);
    assert!(
        catalog(server.endpoint.clone())
            .await
            .get(permission)
            .is_some()
    );
    assert_eq!(get(alice.clone()).await, StatusCode::FORBIDDEN);
    let local = alice.local_authority().unwrap();
    let owner = dhttp_home::certificate::extract_dhttp_subject_key_identifier(local.certificates())
        .unwrap();
    let (status, _) = owner_request(
        &server,
        Method::POST,
        "/sys/admin",
        serde_json::json!({"name":alice.name(),"subject_id":owner.owner_hash().as_str()}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(get(alice.clone()).await, StatusCode::OK);
    assert_eq!(
        catalog(alice.clone()).await[permission]["management"],
        "administrator"
    );
    assert_eq!(get(bob.clone()).await, StatusCode::FORBIDDEN);
    assert!(catalog(bob).await.get(permission).is_none());
    let (status, administrators) =
        owner_request(&server, Method::GET, "/sys/admin", serde_json::Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let added = administrators
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "alice.dhttp.net")
        .unwrap();
    assert_eq!(added.as_object().unwrap().len(), 2);
    assert!(added.get("subject_id").is_some());
    assert_eq!(
        owner_request(
            &server,
            Method::DELETE,
            "/sys/admin?name=alice",
            serde_json::Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(get(alice.clone()).await, StatusCode::FORBIDDEN);
    assert!(catalog(alice).await.get(permission).is_none());
    server.close().await.unwrap();
}
