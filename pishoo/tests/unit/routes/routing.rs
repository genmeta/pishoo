use super::*;

fn test_router(
    _endpoint: dhttp::Endpoint,
    access: Arc<AccessService>,
    config: &ServerConfig,
    profile: &dhttp_home::identity::IdentityProfile,
) -> Router {
    let proxies = config.proxy_locations.clone();
    Router::new()
        .merge(access_router(access.clone()))
        .merge(file_router(profile.join("file")))
        .fallback(axum::routing::any(move |request: Request<AxumBody>| {
            proxy_pass(proxies.clone(), request)
        }))
        .layer(axum::middleware::from_fn_with_state(access, authorize))
}

#[test]
fn proxy_preserves_absent_path_and_replaces_explicit_path() {
    let uri = "https://alice.dhttp.net/prefix/path?x=1".parse().unwrap();
    let mut parts = "http://127.0.0.1:8080"
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
        "http://127.0.0.1:8080/prefix/path?x=1"
    );
    route.proxy_pass.path_and_query = Some("/".parse().unwrap());
    assert_eq!(
        proxy_uri(&route, &uri).unwrap(),
        "http://127.0.0.1:8080/path?x=1"
    );
    route.proxy_pass.path_and_query = Some("/up".parse().unwrap());
    assert_eq!(
        proxy_uri(&route, &uri).unwrap(),
        "http://127.0.0.1:8080/uppath?x=1"
    );
}

#[tokio::test]
async fn proxy_forwards_request_and_response_through_local_http_tcp() {
    use std::convert::Infallible;

    use bytes::Bytes;
    use http_body_util::Full;
    use hyper::{body::Incoming, service::service_fn};
    use hyper_util::rt::TokioIo;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let service = service_fn(move |request: Request<Incoming>| async move {
            assert_eq!(request.method(), Method::POST);
            assert_eq!(request.uri(), "/api/echo?x=1");
            assert_eq!(
                request.headers()[header::HOST].to_str().unwrap(),
                address.to_string()
            );
            assert_eq!(request.headers()["x-custom"], "hello");
            assert!(!request.headers().contains_key("x-drop"));
            assert!(!request.headers().contains_key("x-forwarded-host"));
            let body = request.into_body().collect().await.unwrap().to_bytes();
            let mut response = http::Response::builder()
                .header("connection", "x-response-drop")
                .header("x-response-drop", "hidden")
                .body(Full::new(body))
                .unwrap();
            response
                .headers_mut()
                .append("set-cookie", "a=1".parse().unwrap());
            response
                .headers_mut()
                .append("set-cookie", "b=2".parse().unwrap());
            Ok::<_, Infallible>(response)
        });
        hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(socket), service)
            .await
            .unwrap();
    });
    let route = ProxyLocation {
        location: "/relay/".into(),
        proxy_pass: format!("http://{address}/api/")
            .parse::<http::Uri>()
            .unwrap()
            .into_parts(),
    };
    let request = Request::builder()
        .method(Method::POST)
        .uri("https://owner.dhttp.net/relay/echo?x=1")
        .header(header::HOST, "owner.dhttp.net")
        .header("connection", "x-drop")
        .header("x-drop", "hidden")
        .header("x-custom", "hello")
        .body(
            Full::new(Bytes::from_static(b"hello"))
                .map_err(|never| match never {})
                .boxed_unsync(),
        )
        .unwrap();
    let response = proxy(route, request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key("connection"));
    assert!(!response.headers().contains_key("x-response-drop"));
    assert_eq!(response.headers().get_all("set-cookie").iter().count(), 2);
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "hello"
    );
    server.abort();
}

#[tokio::test]
async fn proxy_streams_response_chunks_before_upload_finishes() {
    use std::{convert::Infallible, time::Duration};

    use bytes::Bytes;
    use http_body::Frame;
    use http_body_util::StreamBody;
    use hyper::{body::Incoming, service::service_fn};
    use hyper_util::rt::TokioIo;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let service = service_fn(|request: Request<Incoming>| async move {
            let (reply_tx, reply_rx) = tokio::sync::mpsc::channel(2);
            tokio::spawn(async move {
                let mut upload = request.into_body();
                while let Some(frame) = upload.frame().await {
                    let frame = frame.unwrap();
                    if let Ok(bytes) = frame.into_data()
                        && reply_tx
                            .send(Ok::<_, Infallible>(Frame::data(bytes)))
                            .await
                            .is_err()
                    {
                        break;
                    }
                }
            });
            let replies = futures::stream::unfold(reply_rx, |mut rx| async move {
                rx.recv().await.map(|frame| (frame, rx))
            });
            Ok::<_, Infallible>(http::Response::new(StreamBody::new(replies)))
        });
        hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(socket), service)
            .await
            .unwrap();
    });
    let route = ProxyLocation {
        location: "/stream".into(),
        proxy_pass: format!("http://{address}")
            .parse::<http::Uri>()
            .unwrap()
            .into_parts(),
    };
    let (upload_tx, upload_rx) = tokio::sync::mpsc::channel(2);
    let upload = futures::stream::unfold(upload_rx, |mut rx| async move {
        rx.recv().await.map(|frame| (frame, rx))
    });
    let request = Request::builder()
        .method(Method::POST)
        .uri("https://owner.dhttp.net/stream")
        .body(StreamBody::new(upload).boxed_unsync())
        .unwrap();
    let mut response = tokio::time::timeout(Duration::from_secs(5), proxy(route, request))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    for chunk in ["first\n", "second\n"] {
        upload_tx
            .send(Ok::<_, dhttp::BoxError>(Frame::data(
                Bytes::copy_from_slice(chunk.as_bytes()),
            )))
            .await
            .unwrap();
        let frame = tokio::time::timeout(Duration::from_secs(5), response.body_mut().frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(frame.into_data().unwrap(), chunk);
        if chunk == "first\n" {
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(31)).await;
            tokio::time::resume();
        }
    }
    drop(upload_tx);
    tokio::time::timeout(Duration::from_secs(5), response.into_body().collect())
        .await
        .unwrap()
        .unwrap();
    server.abort();
}

#[tokio::test]
async fn static_handles_streaming_head_and_directory_confinement() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("index.html"), "home").unwrap();
    let req = Request::builder()
        .uri("/file/index.html")
        .body(AxumBody::empty())
        .unwrap();
    let response = static_file(root.path(), req).await.unwrap();
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "home"
    );
    let req = Request::builder()
        .method(Method::HEAD)
        .uri("/file/index.html")
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
    for path in ["/file/%2e%2e/secret", "/file/missing", "/file/nested/"] {
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
            .uri("/file/escape")
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
    headers.insert("x-custom", "ordinary-value".parse().unwrap());
    headers.append("set-cookie", "a=1".parse().unwrap());
    headers.append("set-cookie", "b=2".parse().unwrap());
    clean_hop_headers(&mut headers);
    assert!(!headers.contains_key("connection") && !headers.contains_key("x-private"));
    assert_eq!(headers["x-custom"], "ordinary-value");
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
    let app = test_router(
        crate::test_identity::endpoint("owner"),
        access,
        &ServerConfig {
            listen: 0,
            exec: false,
            proxy_locations: ["/", "/docs/"]
                .into_iter()
                .map(|location| ProxyLocation {
                    location: location.into(),
                    proxy_pass: "http://127.0.0.1:8080/"
                        .parse::<http::Uri>()
                        .unwrap()
                        .into_parts(),
                })
                .collect(),
        },
        &profile,
    );
    let mut request = anonymous_request();
    *request.uri_mut() = "https://owner.dhttp.net/docs?x=1".parse().unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::MOVED_PERMANENTLY);
    assert_eq!(response.headers()[header::LOCATION], "/docs/?x=1");
}

#[tokio::test]
async fn file_route_is_separate_from_proxy_fallback() {
    use tower::ServiceExt;

    let (access, _) = authorization_app(access_control::Effect::Allow).await;
    for path in ["/file", "/file/", "/file/hello.txt", "/hello.txt"] {
        access
            .set_policy(
                access_control::Method::Unspecified,
                path,
                access_control::Effect::Allow,
                access_control::Grantee::Anony,
            )
            .await
            .unwrap();
    }
    let directory = tempfile::tempdir().unwrap();
    let home = dhttp_home::DhttpHome::new(directory.path().to_path_buf());
    let profile = home.identity_profile("owner").unwrap();
    std::fs::create_dir_all(profile.join("file")).unwrap();
    std::fs::write(profile.join("file/hello.txt"), "hello").unwrap();
    let endpoint = crate::test_identity::endpoint("owner");
    let app = test_router(
        endpoint.clone(),
        access.clone(),
        &ServerConfig {
            listen: 0,
            exec: false,
            proxy_locations: Vec::new(),
        },
        &profile,
    );
    for (path, expected) in [
        ("/file", StatusCode::NOT_FOUND),
        ("/file/", StatusCode::NOT_FOUND),
        ("/hello.txt", StatusCode::NOT_FOUND),
        ("/file/hello.txt", StatusCode::OK),
    ] {
        let mut request = anonymous_request();
        *request.uri_mut() = format!("https://owner.dhttp.net{path}").parse().unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), expected, "{path}");
        if expected == StatusCode::OK {
            assert_eq!(
                response.into_body().collect().await.unwrap().to_bytes(),
                "hello"
            );
        }
    }
    let root_proxy = test_router(
        endpoint,
        access,
        &ServerConfig {
            listen: 0,
            exec: false,
            proxy_locations: vec![ProxyLocation {
                location: "/".into(),
                proxy_pass: "http://127.0.0.1:8080/"
                    .parse::<http::Uri>()
                    .unwrap()
                    .into_parts(),
            }],
        },
        &profile,
    );
    let mut request = anonymous_request();
    *request.uri_mut() = "https://owner.dhttp.net/file/hello.txt".parse().unwrap();
    let response = root_proxy.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "hello"
    );
}

#[tokio::test]
async fn proxy_prefix_matches_path_segments_only() {
    use tower::ServiceExt;

    let (access, _) = authorization_app(access_control::Effect::Allow).await;
    for path in ["/docs", "/docs2"] {
        access
            .set_policy(
                access_control::Method::Unspecified,
                path,
                access_control::Effect::Allow,
                access_control::Grantee::Anony,
            )
            .await
            .unwrap();
    }
    let directory = tempfile::tempdir().unwrap();
    let home = dhttp_home::DhttpHome::new(directory.path().to_path_buf());
    let profile = home.identity_profile("owner").unwrap();
    let app = test_router(
        crate::test_identity::endpoint("owner"),
        access,
        &ServerConfig {
            listen: 0,
            exec: false,
            proxy_locations: vec![ProxyLocation {
                location: "/docs/".into(),
                proxy_pass: "http://127.0.0.1:8080/"
                    .parse::<http::Uri>()
                    .unwrap()
                    .into_parts(),
            }],
        },
        &profile,
    );
    let mut request = anonymous_request();
    *request.uri_mut() = "https://owner.dhttp.net/docs".parse().unwrap();
    assert_eq!(
        app.clone().oneshot(request).await.unwrap().status(),
        StatusCode::MOVED_PERMANENTLY
    );
    let mut request = anonymous_request();
    *request.uri_mut() = "https://owner.dhttp.net/docs2".parse().unwrap();
    assert_eq!(
        app.oneshot(request).await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn api_namespace_never_falls_through_to_static_files_or_root_proxy() {
    use tower::ServiceExt;
    let (access, _) = authorization_app(access_control::Effect::Allow).await;
    for path in ["/api", "/api/", "/api/missing"] {
        access
            .set_policy(
                access_control::Method::Unspecified,
                path,
                access_control::Effect::Allow,
                access_control::Grantee::Anony,
            )
            .await
            .unwrap();
    }
    let directory = tempfile::tempdir().unwrap();
    let home = dhttp_home::DhttpHome::new(directory.path().to_path_buf());
    let profile = home.identity_profile("owner").unwrap();
    let api_directory = profile.join("file/api");
    std::fs::create_dir_all(&api_directory).unwrap();
    std::fs::write(api_directory.join("index.html"), "private static content").unwrap();
    std::fs::write(api_directory.join("missing"), "private static content").unwrap();
    let endpoint = crate::test_identity::endpoint("owner");
    for proxy_locations in [
        Vec::new(),
        vec![ProxyLocation {
            location: "/".into(),
            proxy_pass: "http://127.0.0.1:8080/"
                .parse::<http::Uri>()
                .unwrap()
                .into_parts(),
        }],
    ] {
        let app = test_router(
            endpoint.clone(),
            access.clone(),
            &ServerConfig {
                listen: 0,
                exec: false,
                proxy_locations,
            },
            &profile,
        );
        for path in ["/api", "/api/", "/api/missing"] {
            for method in [Method::GET, Method::HEAD, Method::POST, Method::OPTIONS] {
                let mut request = anonymous_request();
                *request.uri_mut() = format!("https://owner.dhttp.net{path}").parse().unwrap();
                *request.method_mut() = method;
                let response = app.clone().oneshot(request).await.unwrap();
                assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
            }
        }
    }
}
