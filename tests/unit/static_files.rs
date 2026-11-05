use super::*;
use http_body_util::BodyExt;
use std::io::Write;

fn fixture(bytes: &[u8]) -> tempfile::NamedTempFile {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(bytes).unwrap();
    file
}

fn request(method: Method, fields: &[(&str, &str)]) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri("/std/file/example.txt");
    for (key, value) in fields {
        builder = builder.header(*key, *value);
    }
    builder.body(Body::empty()).unwrap()
}

fn serve(file: &tempfile::NamedTempFile, method: Method, fields: &[(&str, &str)]) -> Response {
    response(
        File::open(file.path()).unwrap(),
        "example.txt",
        &request(method, fields),
    )
    .unwrap()
}

async fn bytes(response: Response) -> bytes::Bytes {
    response.into_body().collect().await.unwrap().to_bytes()
}

#[tokio::test]
async fn single_ranges_return_exact_bytes_and_lengths() {
    let file = fixture(b"0123456789");
    for (range, expected, content_range) in [
        ("bytes=0-3", "0123", "bytes 0-3/10"),
        ("bytes=4-", "456789", "bytes 4-9/10"),
        ("bytes=-3", "789", "bytes 7-9/10"),
        ("bytes=0-99", "0123456789", "bytes 0-9/10"),
        ("bytes=-99", "0123456789", "bytes 0-9/10"),
        ("bytes=9-9", "9", "bytes 9-9/10"),
        ("bytes=0004-0006", "456", "bytes 4-6/10"),
        ("BYTES= 1-2", "12", "bytes 1-2/10"),
        (
            "bytes=0-999999999999999999999999999999",
            "0123456789",
            "bytes 0-9/10",
        ),
        (
            "bytes=-999999999999999999999999999999",
            "0123456789",
            "bytes 0-9/10",
        ),
    ] {
        let response = serve(&file, Method::GET, &[("range", range)]);
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT, "{range}");
        assert_eq!(response.headers()[header::CONTENT_RANGE], content_range);
        assert_eq!(
            response.headers()[header::CONTENT_LENGTH],
            expected.len().to_string()
        );
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "private, no-cache"
        );
        assert_eq!(bytes(response).await, expected, "{range}");
    }
}

#[tokio::test]
async fn unsatisfiable_ranges_and_empty_files_have_valid_framing() {
    let file = fixture(b"0123456789");
    for range in [
        "bytes=10-",
        "bytes=11-20",
        "bytes=-0",
        "bytes=999999999999999999999999999999-",
    ] {
        let response = serve(&file, Method::GET, &[("range", range)]);
        assert_eq!(
            response.status(),
            StatusCode::RANGE_NOT_SATISFIABLE,
            "{range}"
        );
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes */10");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "0");
        assert_eq!(
            response
                .headers()
                .get_all(header::CACHE_CONTROL)
                .iter()
                .count(),
            1
        );
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert!(bytes(response).await.is_empty());
    }
    let empty = fixture(b"");
    for range in ["bytes=0-0", "bytes=-1", "bytes=-0"] {
        let response = serve(&empty, Method::GET, &[("range", range)]);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "0");
        assert!(!response.headers().contains_key(header::CONTENT_RANGE));
        assert!(bytes(response).await.is_empty());
    }
}

#[tokio::test]
async fn unsupported_invalid_and_multiple_ranges_fall_back_to_full_responses() {
    let file = fixture(b"0123456789");
    for range in [
        "items=0-2",
        "bytes=3-1",
        "bytes=",
        "bytes=-",
        "bytes=x-2",
        "bytes=1-x",
        "bytes=+1-2",
        "bytes=1-2-3",
        "bytes=1 -2",
        "bytes=1- 2",
        "bytes=0-1,7-8",
        "bytes=0-1,99-",
        "bytes=999999999999999999999999-888888888888888888888888",
    ] {
        let response = serve(&file, Method::GET, &[("range", range)]);
        assert_eq!(response.status(), StatusCode::OK, "{range}");
        assert!(!response.headers().contains_key(header::CONTENT_RANGE));
        assert_eq!(bytes(response).await, "0123456789");
    }
    let response = serve(
        &file,
        Method::GET,
        &[("range", "bytes=0-1"), ("range", "bytes=7-8")],
    );
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(bytes(response).await, "0123456789");
}

#[tokio::test]
async fn conditional_get_reuses_weak_etags_and_precedes_range() {
    let file = fixture(b"content");
    let initial = serve(&file, Method::GET, &[]);
    let etag = initial.headers()[header::ETAG].to_str().unwrap().to_owned();
    assert!(etag.starts_with("W/\""));
    assert_eq!(initial.headers()[header::ACCEPT_RANGES], "bytes");
    assert_eq!(initial.headers()[header::CONTENT_TYPE], "text/plain");
    assert_eq!(initial.headers()["x-content-type-options"], "nosniff");
    assert!(initial.headers().contains_key(header::DATE));
    assert_eq!(bytes(initial).await, "content");
    for value in [
        etag.clone(),
        etag[2..].to_owned(),
        "*".into(),
        format!(", \"other,tag\", {etag},"),
    ] {
        let response = serve(
            &file,
            Method::GET,
            &[("if-none-match", &value), ("range", "bytes=99-")],
        );
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED, "{value}");
        assert_eq!(response.headers()[header::ETAG], etag);
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "private, no-cache"
        );
        assert!(response.headers().contains_key(header::LAST_MODIFIED));
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "7");
        assert!(!response.headers().contains_key(header::CONTENT_RANGE));
        assert!(bytes(response).await.is_empty());
    }
    let response = serve(
        &file,
        Method::GET,
        &[("if-none-match", "\"other\""), ("if-none-match", &etag)],
    );
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
}

#[tokio::test]
async fn preconditions_have_rfc_precedence_and_strong_comparison() {
    let file = fixture(b"content");
    let headers = serve(&file, Method::HEAD, &[]).headers().clone();
    let etag = headers[header::ETAG].to_str().unwrap();
    let modified = headers[header::LAST_MODIFIED].to_str().unwrap();
    let old = "Thu, 01 Jan 1970 00:00:00 GMT";
    for fields in [
        vec![("if-match", etag), ("if-none-match", "*")],
        vec![("if-match", &etag[2..]), ("range", "bytes=0-1")],
        vec![("if-unmodified-since", old), ("if-none-match", "*")],
    ] {
        let response = serve(&file, Method::GET, &fields);
        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "0");
        assert!(bytes(response).await.is_empty());
    }
    let response = serve(
        &file,
        Method::GET,
        &[("if-match", "*"), ("if-unmodified-since", old)],
    );
    assert_eq!(response.status(), StatusCode::OK);
    let response = serve(
        &file,
        Method::GET,
        &[
            ("if-none-match", "\"different\""),
            ("if-modified-since", modified),
        ],
    );
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(bytes(response).await, "content");
    let response = serve(
        &file,
        Method::GET,
        &[("if-none-match", etag), ("if-modified-since", old)],
    );
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
}

#[tokio::test]
async fn dates_support_http_formats_invalid_values_and_single_value_fields() {
    let file = fixture(b"content");
    let modified = UNIX_EPOCH + Duration::from_secs(784111777);
    file.as_file()
        .set_times(std::fs::FileTimes::new().set_modified(modified))
        .unwrap();
    for value in [
        "Sun, 06 Nov 1994 08:49:37 GMT",
        "Sunday, 06-Nov-94 08:49:37 GMT",
        "Sun Nov  6 08:49:37 1994",
    ] {
        let response = serve(&file, Method::GET, &[("if-modified-since", value)]);
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED, "{value}");
    }
    for fields in [
        vec![("if-modified-since", "invalid")],
        vec![("if-unmodified-since", "invalid")],
        vec![
            ("if-modified-since", "Sun, 06 Nov 1994 08:49:37 GMT"),
            ("if-modified-since", "Sun, 06 Nov 1994 08:49:37 GMT"),
        ],
        vec![("if-modified-since", "Thu, 01 Jan 1970 00:00:00 GMT")],
    ] {
        assert_eq!(serve(&file, Method::GET, &fields).status(), StatusCode::OK);
    }
    file.as_file()
        .set_times(
            std::fs::FileTimes::new().set_modified(SystemTime::now() + Duration::from_secs(86400)),
        )
        .unwrap();
    let response = serve(&file, Method::HEAD, &[]);
    assert!(
        httpdate::parse_http_date(response.headers()[header::LAST_MODIFIED].to_str().unwrap())
            .unwrap()
            <= httpdate::parse_http_date(response.headers()[header::DATE].to_str().unwrap())
                .unwrap()
    );
}

#[tokio::test]
async fn malformed_entity_tags_are_rejected_even_after_a_matching_tag() {
    let file = fixture(b"content");
    for name in ["if-match", "if-none-match"] {
        for value in [
            "",
            ",,,",
            "unquoted",
            "w/\"tag\"",
            "\"unclosed",
            "\"tag\" trailing",
            "*, \"tag\"",
            "\"tag\", *",
            "\"ta\tg\"",
        ] {
            assert!(
                matches!(
                    response(
                        File::open(file.path()).unwrap(),
                        "example.txt",
                        &request(Method::GET, &[(name, value)])
                    ),
                    Err(Error::BadRequest(_))
                ),
                "{name}: {value}"
            );
        }
    }
    let etag = serve(&file, Method::HEAD, &[]).headers()[header::ETAG]
        .to_str()
        .unwrap()
        .to_owned();
    let value = format!("{etag}, invalid");
    assert!(matches!(
        response(
            File::open(file.path()).unwrap(),
            "example.txt",
            &request(Method::GET, &[("if-none-match", &value)])
        ),
        Err(Error::BadRequest(_))
    ));
}

#[tokio::test]
async fn head_and_conditional_hits_do_not_require_a_readable_file_handle() {
    let file = fixture(b"content");
    let etag = serve(&file, Method::HEAD, &[]).headers()[header::ETAG]
        .to_str()
        .unwrap()
        .to_owned();
    for (method, fields, status) in [
        (Method::HEAD, vec![("range", "bytes=99-")], StatusCode::OK),
        (
            Method::HEAD,
            vec![("if-range", "invalid"), ("range", "bytes=0-1")],
            StatusCode::OK,
        ),
        (
            Method::HEAD,
            vec![("if-none-match", etag.as_str())],
            StatusCode::NOT_MODIFIED,
        ),
        (
            Method::GET,
            vec![("if-none-match", etag.as_str())],
            StatusCode::NOT_MODIFIED,
        ),
    ] {
        let write_only = File::options().write(true).open(file.path()).unwrap();
        let response = response(write_only, "example.txt", &request(method, &fields)).unwrap();
        assert_eq!(response.status(), status);
        if status == StatusCode::OK {
            assert_eq!(response.headers()[header::CONTENT_LENGTH], "7");
        }
        assert!(bytes(response).await.is_empty());
    }
}

#[tokio::test]
async fn if_range_without_a_strong_validator_sends_the_complete_representation() {
    let file = fixture(b"0123456789");
    let headers = serve(&file, Method::HEAD, &[]).headers().clone();
    let etag = headers[header::ETAG].to_str().unwrap();
    for value in [
        etag,
        &etag[2..],
        headers[header::LAST_MODIFIED].to_str().unwrap(),
        "invalid",
    ] {
        let response = serve(
            &file,
            Method::GET,
            &[("range", "bytes=3-5"), ("if-range", value)],
        );
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key(header::CONTENT_RANGE));
        assert_eq!(bytes(response).await, "0123456789");
    }
    assert_eq!(
        serve(
            &file,
            Method::GET,
            &[
                ("if-none-match", etag),
                ("range", "bytes=3-5"),
                ("if-range", "invalid")
            ]
        )
        .status(),
        StatusCode::NOT_MODIFIED
    );
}

#[tokio::test]
async fn atomic_replacement_with_identical_size_and_mtime_invalidates_the_tag() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("example.txt");
    std::fs::write(&path, b"first").unwrap();
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    let initial = response(
        File::open(&path).unwrap(),
        "example.txt",
        &request(Method::GET, &[]),
    )
    .unwrap();
    let etag = initial.headers()[header::ETAG].to_str().unwrap().to_owned();
    let replacement = root.path().join("new.txt");
    std::fs::write(&replacement, b"other").unwrap();
    File::open(&replacement)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(modified))
        .unwrap();
    std::fs::rename(&replacement, &path).unwrap();
    let current = response(
        File::open(&path).unwrap(),
        "example.txt",
        &request(Method::GET, &[("if-none-match", &etag)]),
    )
    .unwrap();
    assert_eq!(current.status(), StatusCode::OK);
    assert_ne!(current.headers()[header::ETAG], etag);
    assert_eq!(bytes(initial).await, "first");
    assert_eq!(bytes(current).await, "other");
}

#[tokio::test]
async fn sparse_file_range_seeks_past_four_gib_without_reading_the_prefix() {
    let mut file = fixture(b"");
    let offset = 5 * 1024 * 1024 * 1024u64;
    file.as_file().set_len(offset).unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    file.write_all(b"tail").unwrap();
    let range = format!("bytes={offset}-");
    let response = serve(&file, Method::GET, &[("range", &range)]);
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()[header::CONTENT_LENGTH], "4");
    assert_eq!(bytes(response).await, "tail");
}

#[tokio::test]
async fn streams_bound_growth_and_report_premature_eof() {
    let mut file = fixture(b"0123456789");
    let response = serve(&file, Method::GET, &[]);
    file.write_all(b"additional bytes").unwrap();
    assert_eq!(bytes(response).await, "0123456789");
    for fields in [vec![], vec![("range", "bytes=3-8")]] {
        file.as_file().set_len(10).unwrap();
        let response = serve(&file, Method::GET, &fields);
        file.as_file().set_len(5).unwrap();
        assert!(response.into_body().collect().await.is_err());
    }
}

#[tokio::test]
async fn large_responses_keep_sixteen_kib_chunks() {
    let file = fixture(&vec![42; 128 * 1024 + 1]);
    let mut body = serve(&file, Method::GET, &[]).into_body();
    let mut count = 0;
    while let Some(frame) = body.frame().await {
        let data = frame.unwrap().into_data().unwrap();
        assert!(data.len() <= 16 * 1024);
        count += data.len();
    }
    assert_eq!(count, 128 * 1024 + 1);
}
