use super::*;

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

fn anonymous_request() -> Request<AxumBody> {
    let name = "owner.dhttp.net";
    let cert = rcgen::generate_simple_self_signed(vec![name.into()]).unwrap();
    let local = dhttp::LocalAuthority::new(
        &qtls::default_provider(),
        Arc::from(name),
        vec![cert.cert.der().clone()],
        qtls::PrivateKeyDer::try_from(cert.signing_key.serialize_der()).unwrap(),
        vec![1],
    )
    .unwrap();
    let mut request = Request::builder()
        .uri("https://owner.dhttp.net/protected?x=1")
        .body(AxumBody::empty())
        .unwrap();
    request.extensions_mut().insert(dhttp::HandshakeSummary {
        alpn: None,
        local: Some(local),
        remote: None,
    });
    // Old middleware or a forwarded request must not supply authentication.
    request.extensions_mut().insert(Visitor::new(
        "forged.dhttp.net",
        SubjectId::new([2]).unwrap(),
    ));
    request
}

async fn authorization_app(effect: access_control::Effect) -> (Arc<AccessService>, Router) {
    let access = Arc::new(
        AccessService::load_from_db(
            "sqlite::memory:",
            "owner.dhttp.net",
            &SubjectId::new([1]).unwrap(),
        )
        .await
        .unwrap(),
    );
    access
        .set_policy(
            access_control::Method::Unspecified,
            "/protected",
            effect,
            access_control::Grantee::Anony,
        )
        .await
        .unwrap();
    let app = Router::new()
        .route(
            "/protected",
            axum::routing::get(|request: Request<AxumBody>| async move {
                assert!(
                    request.extensions().get::<Visitor>().is_none(),
                    "anonymous request retains no stale Visitor"
                );
                StatusCode::NO_CONTENT
            }),
        )
        .layer(axum::middleware::from_fn_with_state(
            access.clone(),
            authorize,
        ));
    (access, app)
}

async fn pending_review(access: &AccessService) -> u64 {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let (count, reviews) = access.pending_live_reviews(0, 10);
            if count == 1 {
                let access_control::ReviewRecord::Live { id, request } = &reviews[0] else {
                    panic!("expected live review")
                };
                assert!(request.headers().request_id.is_none());
                assert!(request.name().is_none());
                assert_eq!(request.headers().path, "/protected?x=1");
                return *id;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("authorization must register its live review")
}

#[tokio::test]
async fn authorization_uses_current_library_allow_and_deny_results() {
    use tower::ServiceExt;
    for (effect, expected) in [
        (access_control::Effect::Allow, StatusCode::NO_CONTENT),
        (access_control::Effect::Deny, StatusCode::FORBIDDEN),
    ] {
        let (access, app) = authorization_app(effect).await;
        let response = app.oneshot(anonymous_request()).await.unwrap();
        assert_eq!(response.status(), expected);
        assert_eq!(access.pending_live_reviews(0, 10).0, 0);
    }
}

#[tokio::test]
async fn authorization_requires_a_trusted_summary_even_for_anonymous_allow() {
    use tower::ServiceExt;
    let (_, app) = authorization_app(access_control::Effect::Allow).await;
    let mut request = anonymous_request();
    request.extensions_mut().remove::<dhttp::HandshakeSummary>();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn reviews_wait_in_the_request_then_apply_allow_or_deny() {
    use tower::ServiceExt;
    for (action, expected) in [
        (Action::Allow, StatusCode::NO_CONTENT),
        (Action::Deny, StatusCode::FORBIDDEN),
    ] {
        let (access, app) = authorization_app(access_control::Effect::Review).await;
        let response = tokio::spawn(app.oneshot(anonymous_request()));
        let id = pending_review(&access).await;
        assert!(
            !response.is_finished(),
            "a pending review must not return 202 or a final response"
        );
        access
            .decide_review(access_control::ReviewTarget::Live(id), action, None)
            .await
            .unwrap();
        assert_eq!(response.await.unwrap().unwrap().status(), expected);
        assert_eq!(access.pending_live_reviews(0, 10).0, 0);
        assert!(
            !access.reviews().del(id),
            "request cleanup must remove its registry entry"
        );
    }
}

#[tokio::test]
async fn abandoning_a_review_removes_its_live_registration() {
    use tower::ServiceExt;
    let (access, app) = authorization_app(access_control::Effect::Review).await;
    let response = tokio::spawn(app.oneshot(anonymous_request()));
    let id = pending_review(&access).await;
    response.abort();
    assert!(response.await.unwrap_err().is_cancelled());
    assert_eq!(access.pending_live_reviews(0, 10).0, 0);
    assert!(!access.reviews().del(id));
    assert!(
        access
            .decide_review(access_control::ReviewTarget::Live(id), Action::Allow, None)
            .await
            .is_err()
    );
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
