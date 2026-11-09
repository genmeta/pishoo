use tower::ServiceExt;

use super::*;

fn setup() -> (tempfile::TempDir, Router) {
    let dir = tempfile::tempdir().unwrap();
    let api = router(
        Dir::open_ambient_dir(dir.path(), cap_std::ambient_authority()).unwrap(),
        Router::new(),
    );
    (dir, api)
}
async fn call(
    api: &Router,
    method: &str,
    path: &str,
    headers: &[(&str, String)],
    bytes: impl Into<Body>,
) -> Response {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("tus-resumable", "1.0.0");
    for (k, v) in headers {
        request = request.header(*k, v);
    }
    api.clone()
        .oneshot(request.body(bytes.into()).unwrap())
        .await
        .unwrap()
}
async fn make(api: &Router, key: &str, size: u64) -> String {
    let response = call(
        api,
        "POST",
        &format!("{UPLOAD}?request_id={key}&directory=bob.example/2026-11-06"),
        &[
            ("upload-length", size.to_string()),
            (
                "upload-metadata",
                format!(
                    "filename {},filetype {}",
                    STANDARD.encode("测试.txt"),
                    STANDARD.encode("text/plain")
                ),
            ),
        ],
        Body::empty(),
    )
    .await;
    let status = response.status();
    let headers = response.headers().clone();
    assert_eq!(
        status,
        201,
        "{:?}",
        response.into_body().collect().await.unwrap().to_bytes()
    );
    headers[header::LOCATION]
        .to_str()
        .unwrap()
        .split("id=")
        .nth(1)
        .unwrap()
        .to_owned()
}
async fn put(api: &Router, id: &str, offset: u64, bytes: &[u8]) -> Response {
    call(
        api,
        "PATCH",
        &format!("{UPLOAD}?id={id}"),
        &[
            ("content-type", "application/offset+octet-stream".into()),
            ("upload-offset", offset.to_string()),
        ],
        bytes.to_vec(),
    )
    .await
}
fn stored_file(dir: &tempfile::TempDir, id: &str) -> std::path::PathBuf {
    let root = Arc::new(
        Dir::open_ambient_dir(dir.path().join(".file"), cap_std::ambient_authority()).unwrap(),
    );
    let value = vfs::with(root, |db| record(db, id)).unwrap();
    dir.path()
        .join(".file")
        .join(&value.directory)
        .join(value.storage_name.as_deref().unwrap())
}
async fn facts(api: &Router, id: &str) -> Value {
    let r = call(
        api,
        "GET",
        &format!("{FILE}?id={id}&view=meta"),
        &[],
        Body::empty(),
    )
    .await;
    assert_eq!(r.status(), 200);
    serde_json::from_slice(&r.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

#[tokio::test]
async fn resume_digest_idempotence_and_immutable_completion() {
    let (_dir, api) = setup();
    let data: Vec<u8> = (0..13017).map(|n| (n % 251) as u8).collect();
    let id = make(&api, "stable", data.len() as u64).await;
    assert_eq!(make(&api, "stable", data.len() as u64).await, id);
    let conflict = call(
        &api,
        "POST",
        &format!("{UPLOAD}?request_id=stable&directory=other/2026-11-06"),
        &[("upload-length", data.len().to_string())],
        Body::empty(),
    )
    .await;
    assert_eq!(conflict.status(), 409);
    for (a, b) in [(0, 17), (17, 1024), (1024, 6001), (6001, data.len())] {
        let r = put(&api, &id, a as u64, &data[a..b]).await;
        assert_eq!(
            r.status(),
            204,
            "{:?}",
            r.into_body().collect().await.unwrap().to_bytes()
        );
        assert_eq!(facts(&api, &id).await["offset"], b);
    }
    let fact = facts(&api, &id).await;
    assert_eq!(fact["digest"], blake3::hash(&data).to_hex().to_string());
    assert!(fact["expires_at"].is_null());
    assert_eq!(
        put(&api, &id, data.len() as u64, b"more").await.status(),
        409
    );
    let response = call(&api, "GET", &format!("{FILE}?id={id}"), &[], Body::empty()).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .as_ref(),
        data
    );
}
#[tokio::test]
async fn zero_byte_file_has_digest_without_patch() {
    let (_d, api) = setup();
    let id = make(&api, "empty", 0).await;
    assert_eq!(
        facts(&api, &id).await["digest"],
        blake3::hash(&[]).to_hex().to_string()
    );
    let r = call(&api, "GET", &format!("{FILE}?id={id}"), &[], Body::empty()).await;
    assert_eq!(r.status(), 200);
    assert!(r.into_body().collect().await.unwrap().to_bytes().is_empty());
}
#[tokio::test]
async fn checksum_failure_does_not_advance_and_sha1_sha256_work() {
    let (_d, api) = setup();
    let id = make(&api, "checksums", 6).await;
    for (algorithm, checksum, status) in [
        ("sha256", vec![0; 32], 460),
        ("sha1", sha1::Sha1::digest(b"abc").to_vec(), 204),
    ] {
        let r = call(
            &api,
            "PATCH",
            &format!("{UPLOAD}?id={id}"),
            &[
                ("content-type", "application/offset+octet-stream".into()),
                ("upload-offset", "0".into()),
                (
                    "upload-checksum",
                    format!("{algorithm} {}", STANDARD.encode(checksum)),
                ),
            ],
            b"abc".to_vec(),
        )
        .await;
        assert_eq!(r.status().as_u16(), status);
        if status == 460 {
            assert_eq!(facts(&api, &id).await["offset"], 0);
        }
    }
    let r = call(
        &api,
        "PATCH",
        &format!("{UPLOAD}?id={id}"),
        &[
            ("content-type", "application/offset+octet-stream".into()),
            ("upload-offset", "3".into()),
            (
                "upload-checksum",
                format!("sha256 {}", STANDARD.encode(sha2::Sha256::digest(b"def"))),
            ),
        ],
        b"def".to_vec(),
    )
    .await;
    assert_eq!(r.status(), 204);
}
#[tokio::test]
async fn body_error_and_oversize_leave_confirmed_offset() {
    let (_d, api) = setup();
    let id = make(&api, "failure", 6).await;
    let stream = futures::stream::iter(vec![
        Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"abc")),
        Err(std::io::Error::other("disconnected")),
    ]);
    let r = call(
        &api,
        "PATCH",
        &format!("{UPLOAD}?id={id}"),
        &[
            ("content-type", "application/offset+octet-stream".into()),
            ("upload-offset", "0".into()),
        ],
        Body::from_stream(stream),
    )
    .await;
    assert_eq!(r.status(), 400);
    assert_eq!(facts(&api, &id).await["offset"], 0);
    assert_eq!(put(&api, &id, 0, b"1234567").await.status(), 413);
    assert_eq!(facts(&api, &id).await["offset"], 0);
}
#[tokio::test]
async fn cancelled_body_removes_own_candidate_without_commit() {
    let (dir, api) = setup();
    let id = make(&api, "cancel", 6).await;
    let stream = async_stream::stream! {yield Ok::<_,std::io::Error>(bytes::Bytes::from_static(b"abc"));std::future::pending::<()>().await;};
    let request = Request::builder()
        .method("PATCH")
        .uri(format!("{UPLOAD}?id={id}"))
        .header("tus-resumable", "1.0.0")
        .header("content-type", "application/offset+octet-stream")
        .header("upload-offset", "0")
        .body(Body::from_stream(stream))
        .unwrap();
    let task = tokio::spawn(api.clone().oneshot(request));
    for _ in 0..100 {
        if dir
            .path()
            .join(".file/temporary")
            .read_dir()
            .is_ok_and(|mut d| d.next().is_some())
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    task.abort();
    let _ = task.await;
    assert_eq!(facts(&api, &id).await["offset"], 0);
    assert_eq!(
        dir.path()
            .join(".file/temporary")
            .read_dir()
            .unwrap()
            .count(),
        0
    );
}
#[tokio::test]
async fn concurrent_same_offset_has_one_winner() {
    let (_d, api) = setup();
    let id = make(&api, "race", 12).await;
    let (a, b) = tokio::join!(put(&api, &id, 0, b"aaaaaa"), put(&api, &id, 0, b"bbbbbb"));
    let mut statuses = [a.status().as_u16(), b.status().as_u16()];
    statuses.sort();
    assert_eq!(statuses, [204, 409]);
    assert_eq!(facts(&api, &id).await["offset"], 6);
}
#[tokio::test]
async fn uncommitted_tail_is_truncated_but_short_content_is_corruption() {
    let (dir, api) = setup();
    let id = make(&api, "tail", 6).await;
    assert_eq!(put(&api, &id, 0, b"abc").await.status(), 204);
    let path = stored_file(&dir, &id);
    std::fs::write(&path, b"abcuncommitted").unwrap();
    assert_eq!(put(&api, &id, 3, b"def").await.status(), 204);
    assert_eq!(std::fs::read(&path).unwrap(), b"abcdef");
    let id = make(&api, "short", 6).await;
    assert_eq!(put(&api, &id, 0, b"abc").await.status(), 204);
    std::fs::write(stored_file(&dir, &id), b"a").unwrap();
    assert_eq!(put(&api, &id, 3, b"def").await.status(), 503);
    assert_eq!(facts(&api, &id).await["offset"], 3);
}
#[tokio::test]
async fn read_range_conditions_and_head_are_consistent() {
    let (_d, api) = setup();
    let id = make(&api, "range", 6).await;
    assert_eq!(
        call(&api, "GET", &format!("{FILE}?id={id}"), &[], Body::empty())
            .await
            .status(),
        409
    );
    assert_eq!(put(&api, &id, 0, b"abcdef").await.status(), 204);
    let r = call(
        &api,
        "GET",
        &format!("{FILE}?id={id}"),
        &[("range", "bytes=1-3".into())],
        Body::empty(),
    )
    .await;
    assert_eq!(r.status(), 206);
    assert_eq!(r.headers()[header::CONTENT_RANGE], "bytes 1-3/6");
    let tag = r.headers()[header::ETAG].to_str().unwrap().to_owned();
    assert_eq!(
        r.into_body().collect().await.unwrap().to_bytes().as_ref(),
        b"bcd"
    );
    for (headers, status, len) in [
        (vec![("if-none-match", tag.clone())], 304, 6),
        (vec![("if-match", "\"wrong\"".into())], 412, 0),
        (vec![("range", "bytes=9-".into())], 416, 0),
        (
            vec![
                ("range", "bytes=1-3".into()),
                ("if-range", "\"wrong\"".into()),
            ],
            200,
            6,
        ),
    ] {
        let r = call(
            &api,
            "GET",
            &format!("{FILE}?id={id}"),
            &headers,
            Body::empty(),
        )
        .await;
        assert_eq!(r.status().as_u16(), status);
        if len != 0 {
            assert_eq!(r.headers()[header::CONTENT_LENGTH], len.to_string());
        }
    }
    let r = call(&api, "HEAD", &format!("{FILE}?id={id}"), &[], Body::empty()).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()[header::CONTENT_LENGTH], "6");
    assert!(r.into_body().collect().await.unwrap().to_bytes().is_empty());
}
#[tokio::test]
async fn roots_are_isolated_for_every_operation_and_query_cannot_select_scope() {
    let (_a, api) = setup();
    let (_b, other) = setup();
    let id = make(&api, "isolation", 3).await;
    put(&api, &id, 0, b"abc").await;
    for (method, path) in [
        ("GET", format!("{FILE}?id={id}")),
        ("HEAD", format!("{FILE}?id={id}")),
        ("GET", format!("{FILE}?id={id}&view=meta")),
        ("PATCH", format!("{UPLOAD}?id={id}")),
        ("DELETE", format!("{UPLOAD}?id={id}")),
    ] {
        assert_eq!(
            call(
                &other,
                method,
                &path,
                &[("range", "bytes=0-1".into()), ("x-lib", "message".into())],
                Body::empty()
            )
            .await
            .status(),
            404
        );
    }
    for tail in [
        format!("id={id}&id={id}"),
        format!("id={id}&lib=message"),
        format!("id={id}&directory=elsewhere"),
        "id=%252e%252e".into(),
        "id=%xx".into(),
        "id=".into(),
        format!("id={id}&index=1"),
    ] {
        assert_eq!(
            call(&api, "GET", &format!("{FILE}?{tail}"), &[], Body::empty())
                .await
                .status(),
            400
        );
    }
}
#[tokio::test]
async fn traversal_symlinks_and_tampered_index_cannot_read_outside() {
    use std::os::unix::fs::symlink;
    let (dir, api) = setup();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), b"private").unwrap();
    for path in [
        "../outside",
        "/absolute",
        "bob//date",
        "bob/./date",
        "bob/%252e%252e",
        "bob%5Csecret",
    ] {
        assert_eq!(
            call(
                &api,
                "POST",
                &format!("{UPLOAD}?request_id=bad&directory={path}"),
                &[("upload-length", "1".into())],
                Body::empty()
            )
            .await
            .status(),
            400
        );
    }
    let id = make(&api, "tamper", 1).await;
    put(&api, &id, 0, b"a").await;
    symlink(outside.path(), dir.path().join(".file/escape")).unwrap();
    let response = call(
        &api,
        "POST",
        &format!("{UPLOAD}?request_id=symlink&directory=escape/sub"),
        &[("upload-length", "1".into())],
        Body::empty(),
    )
    .await;
    assert_eq!(response.status(), 503);
    assert!(!outside.path().join("sub").exists());
    let root = Arc::new(
        Dir::open_ambient_dir(dir.path().join(".file"), cap_std::ambient_authority()).unwrap(),
    );
    vfs::with(root, |db| {
        db.execute("UPDATE files SET directory='../outside' WHERE id=?1", [&id])
            .map_err(storage)?;
        Ok(())
    })
    .unwrap();
    assert_eq!(
        call(&api, "GET", &format!("{FILE}?id={id}"), &[], Body::empty())
            .await
            .status(),
        503
    );
    assert_eq!(
        std::fs::read(outside.path().join("secret")).unwrap(),
        b"private"
    );
}
#[tokio::test]
async fn index_and_journal_symlinks_are_not_opened() {
    use std::os::unix::fs::symlink;
    for name in ["database", "database-journal"] {
        let (dir, api) = setup();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), b"do not touch").unwrap();
        if name == "database-journal" {
            make(&api, "first", 1).await;
        } else {
            std::fs::create_dir(dir.path().join(".file")).unwrap();
        }
        symlink(outside.path(), dir.path().join(".file").join(name)).unwrap();
        let r = call(
            &api,
            "POST",
            &format!("{UPLOAD}?request_id=escape&directory=bob/date"),
            &[("upload-length", "1".into())],
            Body::empty(),
        )
        .await;
        assert_eq!(r.status(), 503);
        assert_eq!(std::fs::read(outside.path()).unwrap(), b"do not touch");
    }
}
#[tokio::test]
async fn expiry_delete_and_reopen_preserve_facts() {
    let (dir, api) = setup();
    let id = make(&api, "restart", 6).await;
    assert_eq!(put(&api, &id, 0, b"abc").await.status(), 204);
    drop(api);
    let api = router(
        Dir::open_ambient_dir(dir.path(), cap_std::ambient_authority()).unwrap(),
        Router::new(),
    );
    assert_eq!(facts(&api, &id).await["offset"], 3);
    let root = Arc::new(
        Dir::open_ambient_dir(dir.path().join(".file"), cap_std::ambient_authority()).unwrap(),
    );
    vfs::with(root, |db| {
        db.execute("UPDATE files SET created_at=1 WHERE id=?1", [&id])
            .map_err(storage)?;
        Ok(())
    })
    .unwrap();
    assert_eq!(
        call(
            &api,
            "HEAD",
            &format!("{UPLOAD}?id={id}"),
            &[],
            Body::empty()
        )
        .await
        .status(),
        410
    );
    assert_eq!(put(&api, &id, 3, b"def").await.status(), 410);
    assert_eq!(
        call(
            &api,
            "DELETE",
            &format!("{UPLOAD}?id={id}"),
            &[],
            Body::empty()
        )
        .await
        .status(),
        204
    );
    assert_eq!(put(&api, &id, 3, b"def").await.status(), 404);
}

static FAULTS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::BTreeMap<String, &'static str>>,
> = std::sync::LazyLock::new(Default::default);
pub(super) fn fault(id: &str, point: &str) -> Result<(), Failure> {
    let mut faults = FAULTS.lock().unwrap();
    if faults.get(id).is_some_and(|p| *p == point) {
        faults.remove(id);
        return Err(fail(503, "injected storage interruption"));
    }
    Ok(())
}
#[tokio::test]
async fn durable_commit_faults_recover_from_the_index_not_file_length() {
    let (dir, api) = setup();
    let id = make(&api, "sync-fault", 6).await;
    FAULTS
        .lock()
        .unwrap()
        .insert(id.clone(), "after_content_sync");
    assert_eq!(put(&api, &id, 0, b"abc").await.status(), 503);
    assert_eq!(facts(&api, &id).await["offset"], 0);
    assert_eq!(std::fs::metadata(stored_file(&dir, &id)).unwrap().len(), 3);
    assert_eq!(put(&api, &id, 0, b"abc").await.status(), 204);
    FAULTS
        .lock()
        .unwrap()
        .insert(id.clone(), "after_index_commit");
    assert_eq!(put(&api, &id, 3, b"def").await.status(), 503);
    drop(api);
    let api = router(
        Dir::open_ambient_dir(dir.path(), cap_std::ambient_authority()).unwrap(),
        Router::new(),
    );
    let value = facts(&api, &id).await;
    assert_eq!(value["offset"], 6);
    assert_eq!(
        value["digest"],
        blake3::hash(b"abcdef").to_hex().to_string()
    );
}
#[tokio::test]
async fn delete_wins_against_patch_waiting_for_network() {
    let (dir, api) = setup();
    let id = make(&api, "delete-race", 6).await;
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let stream = async_stream::stream! {yield Ok::<_,std::io::Error>(bytes::Bytes::from_static(b"abc"));let _=receiver.await;yield Ok(bytes::Bytes::from_static(b"def"));};
    let request = Request::builder()
        .method("PATCH")
        .uri(format!("{UPLOAD}?id={id}"))
        .header("tus-resumable", "1.0.0")
        .header("content-type", "application/offset+octet-stream")
        .header("upload-offset", "0")
        .body(Body::from_stream(stream))
        .unwrap();
    let task = tokio::spawn(api.clone().oneshot(request));
    for _ in 0..100 {
        if dir
            .path()
            .join(".file/temporary")
            .read_dir()
            .is_ok_and(|mut d| d.next().is_some())
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(
        call(
            &api,
            "DELETE",
            &format!("{UPLOAD}?id={id}"),
            &[],
            Body::empty()
        )
        .await
        .status(),
        204
    );
    sender.send(()).unwrap();
    assert_eq!(task.await.unwrap().unwrap().status(), 404);
}
#[tokio::test]
async fn method_override_is_normalized_before_method_routing_and_authorization() {
    let inner = Router::new()
        .route("/only-patch", axum::routing::patch(|| async { "ok" }))
        .layer(axum::middleware::from_fn(
            |request: Request<Body>, next: axum::middleware::Next| async move {
                assert_eq!(request.method(), Method::PATCH);
                assert!(!request.headers().contains_key("x-http-method-override"));
                next.run(request).await
            },
        ));
    let api = Router::new()
        .fallback_service(inner)
        .layer(axum::middleware::from_fn(crate::routes::normalize_method));
    let request = Request::post("/only-patch")
        .header("x-http-method-override", "PATCH")
        .body(Body::empty())
        .unwrap();
    assert_eq!(api.clone().oneshot(request).await.unwrap().status(), 200);
    let request = Request::post("/only-patch")
        .header("x-http-method-override", "PATCH")
        .header("x-http-method-override", "DELETE")
        .body(Body::empty())
        .unwrap();
    assert_eq!(api.oneshot(request).await.unwrap().status(), 400);
}

#[tokio::test]
async fn identical_text_ids_in_two_roots_never_alias_bytes_or_deletion() {
    let (_first, a) = setup();
    let (second, b) = setup();
    let id_a = make(&a, "same-key", 3).await;
    let id_b = make(&b, "same-key", 3).await;
    assert_eq!(put(&a, &id_a, 0, b"abc").await.status(), 204);
    assert_eq!(put(&b, &id_b, 0, b"xyz").await.status(), 204);
    let root = Arc::new(
        Dir::open_ambient_dir(second.path().join(".file"), cap_std::ambient_authority()).unwrap(),
    );
    vfs::with(root, |db| {
        db.execute("UPDATE files SET id=?1 WHERE id=?2", params![id_a, id_b])
            .map_err(storage)?;
        Ok(())
    })
    .unwrap();
    for (api, expected) in [(&a, b"abc"), (&b, b"xyz")] {
        let response = call(api, "GET", &format!("{FILE}?id={id_a}"), &[], Body::empty()).await;
        assert_eq!(response.status(), 200);
        assert_eq!(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .as_ref(),
            expected
        );
    }
    assert_eq!(
        call(
            &b,
            "DELETE",
            &format!("{UPLOAD}?id={id_a}"),
            &[],
            Body::empty()
        )
        .await
        .status(),
        204
    );
    assert_eq!(
        facts(&a, &id_a).await["digest"],
        blake3::hash(b"abc").to_hex().to_string()
    );
    assert_eq!(
        call(&b, "GET", &format!("{FILE}?id={id_a}"), &[], Body::empty())
            .await
            .status(),
        404
    );
}

#[tokio::test]
async fn content_and_candidate_symlinks_do_not_expand_directory_authority() {
    use std::os::unix::fs::symlink;
    let (dir, api) = setup();
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("secret");
    std::fs::write(&secret, b"key").unwrap();
    let id = make(&api, "content-link", 3).await;
    assert_eq!(put(&api, &id, 0, b"abc").await.status(), 204);
    let content = stored_file(&dir, &id);
    std::fs::remove_file(&content).unwrap();
    symlink(&secret, &content).unwrap();
    assert_eq!(
        call(
            &api,
            "GET",
            &format!("{FILE}?id={id}"),
            &[("range", "bytes=0-1".into())],
            Body::empty()
        )
        .await
        .status(),
        503
    );
    assert_eq!(
        call(
            &api,
            "DELETE",
            &format!("{UPLOAD}?id={id}"),
            &[],
            Body::empty()
        )
        .await
        .status(),
        204
    );
    assert_eq!(std::fs::read(&secret).unwrap(), b"key");
    let temporary = dir.path().join(".file/temporary");
    std::fs::remove_dir(&temporary).unwrap();
    symlink(outside.path(), &temporary).unwrap();
    let id = make(&api, "candidate-link", 3).await;
    assert_eq!(put(&api, &id, 0, b"abc").await.status(), 503);
    assert_eq!(facts(&api, &id).await["offset"], 0);
    assert_eq!(outside.path().read_dir().unwrap().count(), 1);
}

#[tokio::test]
async fn concurrent_directory_replacement_never_opens_the_symlink_target() {
    use std::{
        os::unix::fs::symlink,
        sync::atomic::{AtomicBool, Ordering},
    };
    let (dir, api) = setup();
    let outside = tempfile::tempdir().unwrap();
    let id = make(&api, "replace-directory", 3).await;
    assert_eq!(put(&api, &id, 0, b"abc").await.status(), 204);
    let secret_dir = outside.path().join("2026-11-06");
    std::fs::create_dir_all(&secret_dir).unwrap();
    std::fs::write(secret_dir.join("测试.txt"), b"key").unwrap();
    let original = dir.path().join(".file/bob.example");
    let holding = dir.path().join(".file/held-peer");
    let outside_path = outside.path().to_owned();
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let worker = std::thread::spawn(move || {
        let mut replacements = 0;
        while !worker_stop.load(Ordering::SeqCst) {
            std::fs::rename(&original, &holding).unwrap();
            symlink(&outside_path, &original).unwrap();
            std::fs::remove_file(&original).unwrap();
            std::fs::rename(&holding, &original).unwrap();
            replacements += 1;
        }
        replacements
    });
    let cleanup = scopeguard::guard((stop, worker), |(stop, worker)| {
        stop.store(true, Ordering::SeqCst);
        assert!(worker.join().unwrap() > 0);
    });
    for _ in 0..80 {
        let response = call(&api, "GET", &format!("{FILE}?id={id}"), &[], Body::empty()).await;
        assert!(matches!(response.status().as_u16(), 200 | 503));
        if response.status() == 200 {
            assert_eq!(
                response
                    .into_body()
                    .collect()
                    .await
                    .unwrap()
                    .to_bytes()
                    .as_ref(),
                b"abc"
            );
        }
    }
    drop(cleanup);
    assert_eq!(std::fs::read(secret_dir.join("测试.txt")).unwrap(), b"key");
}

#[tokio::test]
async fn completed_files_do_not_expire_when_their_creation_time_is_old() {
    let (dir, api) = setup();
    let id = make(&api, "old-complete", 3).await;
    assert_eq!(put(&api, &id, 0, b"abc").await.status(), 204);
    let root = Arc::new(
        Dir::open_ambient_dir(dir.path().join(".file"), cap_std::ambient_authority()).unwrap(),
    );
    vfs::with(root, |db| {
        db.execute("UPDATE files SET created_at=1 WHERE id=?1", [&id])
            .map_err(storage)?;
        Ok(())
    })
    .unwrap();
    assert!(facts(&api, &id).await["expires_at"].is_null());
    let response = call(&api, "GET", &format!("{FILE}?id={id}"), &[], Body::empty()).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .as_ref(),
        b"abc"
    );
}

#[tokio::test]
async fn original_names_are_direct_files_and_collisions_never_overwrite() {
    let (dir, api) = setup();
    let (a, b) = tokio::join!(make(&api, "first-name", 3), make(&api, "second-name", 3));
    assert_ne!(a, b);
    assert_eq!(put(&api, &a, 0, b"abc").await.status(), 204);
    assert_eq!(put(&api, &b, 0, b"xyz").await.status(), 204);
    let archive = dir.path().join(".file/bob.example/2026-11-06");
    assert!(archive.join("测试.txt").is_file());
    assert!(archive.join("测试 (2).txt").is_file());
    assert!(!archive.join(&a).exists() && !archive.join(&b).exists());
    assert_eq!(std::fs::read(stored_file(&dir, &a)).unwrap(), b"abc");
    assert_eq!(std::fs::read(stored_file(&dir, &b)).unwrap(), b"xyz");
    assert_eq!(make(&api, "first-name", 3).await, a);
    assert_eq!(archive.read_dir().unwrap().count(), 2);
    assert_eq!(facts(&api, &a).await["name"], "测试.txt");
    assert_eq!(facts(&api, &b).await["name"], "测试.txt");
    assert_eq!(
        call(
            &api,
            "DELETE",
            &format!("{UPLOAD}?id={a}"),
            &[],
            Body::empty()
        )
        .await
        .status(),
        204
    );
    assert_eq!(std::fs::read(stored_file(&dir, &b)).unwrap(), b"xyz");
}
#[tokio::test]
async fn filename_cannot_select_a_path_or_follow_existing_symlinks() {
    use std::os::unix::fs::symlink;
    let (dir, api) = setup();
    for name in ["../secret", "/absolute", "dir/file", "dir\\file", ".", ".."] {
        let response = call(
            &api,
            "POST",
            &format!("{UPLOAD}?request_id=bad-name&directory=bob/date"),
            &[
                ("upload-length", "1".into()),
                (
                    "upload-metadata",
                    format!("filename {}", STANDARD.encode(name)),
                ),
            ],
            Body::empty(),
        )
        .await;
        assert_eq!(response.status(), 400, "{name}");
    }
    let outside = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(outside.path(), b"secret").unwrap();
    let archive = dir.path().join(".file/bob.example/2026-11-06");
    std::fs::create_dir_all(&archive).unwrap();
    symlink(outside.path(), archive.join("测试.txt")).unwrap();
    let id = make(&api, "safe-name", 3).await;
    assert_eq!(put(&api, &id, 0, b"abc").await.status(), 204);
    assert_eq!(stored_file(&dir, &id), archive.join("测试 (2).txt"));
    assert_eq!(std::fs::read(outside.path()).unwrap(), b"secret");
    let root = Arc::new(
        Dir::open_ambient_dir(dir.path().join(".file"), cap_std::ambient_authority()).unwrap(),
    );
    vfs::with(root, |db| {
        db.execute(
            "UPDATE files SET storage_name='../secret' WHERE id=?1",
            [&id],
        )
        .map_err(storage)?;
        Ok(())
    })
    .unwrap();
    assert_eq!(
        call(&api, "GET", &format!("{FILE}?id={id}"), &[], Body::empty())
            .await
            .status(),
        503
    );
}
#[test]
fn collision_suffixes_keep_unicode_and_extensions_within_filesystem_limits() {
    let name = format!("{}.txt", "猫".repeat(83));
    assert!(valid_filename(&name));
    let next = collision_name(&name, 2);
    assert!(valid_filename(&next));
    assert!(next.ends_with(" (2).txt"));
    assert_eq!(collision_name(".env", 2), ".env (2)");
    assert_eq!(collision_name("报告.pdf", 3), "报告 (3).pdf");
}
#[tokio::test]
async fn previously_installed_id_layout_can_resume_without_moving_bytes() {
    let (dir, api) = setup();
    let id = "a".repeat(32);
    let archive = dir.path().join(".file/bob/date").join(&id);
    std::fs::create_dir_all(&archive).unwrap();
    std::fs::write(archive.join("content"), b"a").unwrap();
    let root = Arc::new(
        Dir::open_ambient_dir(dir.path().join(".file"), cap_std::ambient_authority()).unwrap(),
    );
    vfs::with(root, |db| {
        db.execute_batch(include_str!(
            "../../fixtures/native-file-id-layout/schema.sql"
        ))
        .map_err(storage)?;
        db.execute(
            "INSERT INTO files VALUES(?1,'old','bob/date','old.txt','text/plain',3,1,?2,NULL,?3)",
            params![
                id,
                "{\"offset\":1,\"stack\":[]}",
                chrono::Utc::now().timestamp()
            ],
        )
        .map_err(storage)?;
        Ok(())
    })
    .unwrap();
    assert_eq!(put(&api, &id, 1, b"bc").await.status(), 204);
    assert_eq!(
        facts(&api, &id).await["digest"],
        blake3::hash(b"abc").to_hex().to_string()
    );
    assert_eq!(std::fs::read(archive.join("content")).unwrap(), b"abc");
}
