use super::*;

#[tokio::test]
async fn file_router_uses_conditional_ranges_directory_indexes_and_uncached_errors() {
    use tower::ServiceExt;

    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("nested")).unwrap();
    std::fs::write(root.path().join("nested/index.html"), b"0123456789").unwrap();
    let router = file_router(root.path().to_owned());
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/std/file/nested/")
                .body(AxumBody::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let etag = response.headers()[header::ETAG]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "private, no-cache"
    );
    assert_eq!(response.headers()[header::CONTENT_TYPE], "text/html");
    for method in [Method::GET, Method::HEAD] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri("/std/file/nested/")
                    .header(header::IF_NONE_MATCH, &etag)
                    .header(header::RANGE, "bytes=99-")
                    .body(AxumBody::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "10");
        assert!(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .is_empty()
        );
    }
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/std/file/nested/")
                .header(header::RANGE, "bytes=4-6")
                .body(AxumBody::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 4-6/10");
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "456"
    );
    for (method, path, condition, status) in [
        (Method::GET, "/std/file", "*", StatusCode::NOT_FOUND),
        (Method::GET, "/std/file/missing", "*", StatusCode::NOT_FOUND),
        (
            Method::GET,
            "/std/file/%2e%2e/secret",
            "*",
            StatusCode::NOT_FOUND,
        ),
        (
            Method::POST,
            "/std/file/nested/",
            "*",
            StatusCode::METHOD_NOT_ALLOWED,
        ),
        (
            Method::GET,
            "/std/file/nested/",
            "invalid",
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header(header::IF_NONE_MATCH, condition)
                    .body(AxumBody::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), status, "{path}");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    }
}
