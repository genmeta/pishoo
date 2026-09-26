#[test]
fn proxy_preserves_absent_path_and_replaces_explicit_path() {
    let uri = "https://alice.dhttp.net/prefix/path?x=1".parse().unwrap();
    let mut parts = "https://bob.dhttp.net"
        .parse::<http::Uri>()
        .unwrap()
        .into_parts();
    parts.path_and_query = None;
    let mut route = ProxyLocation {
        location: "/prefix/".into(),
        proxy_pass: parts,
    };
    assert_eq!(
        proxy_uri(&route, &uri).unwrap(),
        "https://bob.dhttp.net/prefix/path?x=1"
    );
    route.proxy_pass.path_and_query = Some("/".parse().unwrap());
    assert_eq!(
        proxy_uri(&route, &uri).unwrap(),
        "https://bob.dhttp.net/path?x=1"
    );
    route.proxy_pass.path_and_query = Some("/up".parse().unwrap());
    assert_eq!(
        proxy_uri(&route, &uri).unwrap(),
        "https://bob.dhttp.net/uppath?x=1"
    );
}

#[tokio::test]
async fn static_handles_streaming_head_and_directory_confinement() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("index.html"), "home").unwrap();
    let req = Request::builder().uri("/").body(AxumBody::empty()).unwrap();
    let response = static_file(root.path(), req).await.unwrap();
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "home"
    );
    let req = Request::builder()
        .method(Method::HEAD)
        .uri("/")
        .body(AxumBody::empty())
        .unwrap();
    let response = static_file(root.path(), req).await.unwrap();
    assert_eq!(response.headers()[header::CONTENT_LENGTH], "4");
    assert!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .is_empty()
    );
    for path in ["/%2e%2e/secret", "/missing", "/nested/"] {
        let req = Request::builder()
            .uri(path)
            .body(AxumBody::empty())
            .unwrap();
        assert!(matches!(
            static_file(root.path(), req).await,
            Err(Error::RouteNotFound)
        ));
    }
    #[cfg(unix)]
    {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), "secret").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), root.path().join("escape"))
            .unwrap();
        let req = Request::builder()
            .uri("/escape")
            .body(AxumBody::empty())
            .unwrap();
        assert!(matches!(
            static_file(root.path(), req).await,
            Err(Error::RouteNotFound)
        ));
    }
}

#[tokio::test]
async fn workspace_deep_link_and_asset_are_distinct() {
    for (path, expected) in [
        ("/workspace/reviews", StatusCode::OK),
        ("/workspace/missing.js", StatusCode::NOT_FOUND),
    ] {
        let response = workspace(
            Request::builder()
                .uri(path)
                .body(AxumBody::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), expected);
    }
}

#[test]
fn forwarding_keeps_repeated_values_and_removes_connection_tokens() {
    let mut headers = http::HeaderMap::new();
    headers.insert("connection", "x-private".parse().unwrap());
    headers.insert("x-private", "hidden".parse().unwrap());
    headers.insert("pishoo-identity", "spoof".parse().unwrap());
    headers.append("set-cookie", "a=1".parse().unwrap());
    headers.append("set-cookie", "b=2".parse().unwrap());
    clean_hop_headers(&mut headers);
    assert!(
        !headers.contains_key("connection")
            && !headers.contains_key("x-private")
            && !headers.contains_key("pishoo-identity")
    );
    assert_eq!(headers.get_all("set-cookie").iter().count(), 2);
}

#[tokio::test]
async fn trailing_slash_redirect_precedes_the_root_proxy() {
    use tower::ServiceExt;
    let (access, _) = authorization_app(access_control::Effect::Allow).await;
    access
        .set_policy(
            access_control::Method::Unspecified,
            "/docs",
            access_control::Effect::Allow,
            access_control::Grantee::Anony,
        )
        .await
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let home = dhttp_home::DhttpHome::new(directory.path().to_path_buf());
    let profile = home.identity_profile("owner").unwrap();
    let app = build_router(
        dhttp::Endpoint::load("owner").await.unwrap(),
        access,
        &BTreeMap::new(),
        &ServerConfig {
            listen: 0,
            proxy_locations: ["/", "/docs/"]
                .into_iter()
                .map(|location| ProxyLocation {
                    location: location.into(),
                    proxy_pass: "https://upstream.dhttp.net/"
                        .parse::<http::Uri>()
                        .unwrap()
                        .into_parts(),
                })
                .collect(),
        },
        &profile,
        Arc::new(tokio::sync::Semaphore::new(4)),
        TaskTracker::new(),
    )
    .unwrap();
    let mut request = anonymous_request();
    *request.uri_mut() = "https://owner.dhttp.net/docs?x=1".parse().unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::MOVED_PERMANENTLY);
    assert_eq!(response.headers()[header::LOCATION], "/docs/?x=1");
}
