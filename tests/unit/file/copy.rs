use super::*;

fn request(bytes: &'static [u8], directory: &str, digest: &str) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(format!(
            "/std/file/library?directory={directory}&name=report.txt&size=4&digest={digest}"
        ))
        .body(Body::from(bytes))
        .unwrap()
}
fn root(path: &std::path::Path) -> Dir {
    Dir::open_ambient_dir(path, cap_std::ambient_authority()).unwrap()
}

#[tokio::test]
async fn copies_publish_only_verified_content_without_overwriting() {
    let folder = tempfile::tempdir().unwrap();
    let digest = blake3::hash(b"data").to_hex().to_string();
    save(root(folder.path()), request(b"data", "Documents", &digest))
        .await
        .unwrap();
    save(root(folder.path()), request(b"data", "Documents", &digest))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(folder.path().join("Documents/report.txt")).unwrap(),
        b"data"
    );
    assert_eq!(
        std::fs::read(folder.path().join("Documents/report (2).txt")).unwrap(),
        b"data"
    );
    assert_eq!(
        save(root(folder.path()), request(b"oops", "Documents", &digest))
            .await
            .unwrap_err()
            .0,
        409
    );
    assert_eq!(
        std::fs::read_dir(folder.path().join("Documents"))
            .unwrap()
            .count(),
        2
    );
    assert_eq!(
        save(root(folder.path()), request(b"data", "../escape", &digest))
            .await
            .unwrap_err()
            .0,
        400
    );
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), folder.path().join("Downloads")).unwrap();
    assert!(
        save(root(folder.path()), request(b"data", "Downloads", &digest))
            .await
            .is_err()
    );
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn interrupted_copy_removes_partial_file() {
    let folder = tempfile::tempdir().unwrap();
    let digest = blake3::hash(b"data").to_hex().to_string();
    let mut request = request(b"", "Downloads", &digest);
    *request.body_mut() = Body::from_stream(futures::stream::iter([
        Ok(bytes::Bytes::from_static(b"da")),
        Err(std::io::Error::other("cancelled")),
    ]));
    assert_eq!(save(root(folder.path()), request).await.unwrap_err().0, 400);
    assert_eq!(
        std::fs::read_dir(folder.path().join("Downloads"))
            .unwrap()
            .count(),
        0
    );
}
