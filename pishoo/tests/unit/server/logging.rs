use http_body_util::BodyExt;

use super::*;

#[tokio::test]
async fn access_logs_append_per_identity_and_record_all_response_statuses() {
    let root = tempfile::tempdir().unwrap();
    let app = Arc::new(RwLock::new(
        Router::new()
            .route("/ok", any(|| async { http::StatusCode::OK }))
            .route("/denied", any(|| async { http::StatusCode::FORBIDDEN }))
            .route("/failed", any(|| async { http::StatusCode::BAD_GATEWAY })),
    ));
    for identity in ["alice", "bob"] {
        let profile = IdentityProfile::try_from(root.path().join(identity)).unwrap();
        std::fs::create_dir_all(profile.join("logs")).unwrap();
        let path = profile.join("logs/access.log");
        {
            let log = std::sync::Mutex::new(open_access_log(&profile).unwrap());
            let requests = (0..24).map(|index| {
                let app = app.clone();
                let log = &log;
                async move {
                    let (path, status) = match index % 4 {
                        0 => ("/ok?ignored=query", 200),
                        1 => ("/denied", 403),
                        2 => ("/missing", 404),
                        _ => ("/failed", 502),
                    };
                    let request = Request::builder()
                        .uri(path)
                        .version(http::Version::HTTP_3)
                        .body(
                            http_body_util::Empty::<bytes::Bytes>::new()
                                .map_err(|never| match never {})
                                .boxed_unsync(),
                        )
                        .unwrap();
                    assert_eq!(
                        serve_request(app, request, identity, log).await.status(),
                        status
                    );
                }
            });
            futures::future::join_all(requests).await;
        }
        // Reopening a listener must preserve all previous requests.
        drop(open_access_log(&profile).unwrap());
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.lines().count(), 24);
        assert!(!contents.contains("ignored=query"));
        for line in contents.lines() {
            assert!(line.contains(&format!(" request server={identity} peer=anonymous ")));
            assert!(line.contains("version=HTTP/3.0 method=GET path="));
            assert!(
                line.ends_with("status=200")
                    || line.ends_with("status=403")
                    || line.ends_with("status=404")
                    || line.ends_with("status=502")
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}

#[test]
fn access_log_open_failure_reports_the_identity_path() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.join("logs/access.log")).unwrap();
    let error = open_access_log(&profile).unwrap_err();
    assert!(
        error
            .to_string()
            .contains(profile.join("logs/access.log").to_str().unwrap())
    );
}

#[tokio::test]
async fn server_errors_and_access_log_write_failures_are_tracing_errors() {
    use tracing::instrument::WithSubscriber;

    let root = tempfile::tempdir().unwrap();
    let error_path = root.path().join("error.log");
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::ERROR)
        .with_writer(std::sync::Mutex::new(
            std::fs::File::create(&error_path).unwrap(),
        ))
        .finish();
    let access_path = root.path().join("access.log");
    std::fs::write(&access_path, "existing\n").unwrap();
    let log = std::sync::Mutex::new(std::fs::File::open(&access_path).unwrap());
    let app = Arc::new(RwLock::new(
        Router::new().fallback(|| async { http::StatusCode::INTERNAL_SERVER_ERROR }),
    ));
    let request = Request::builder()
        .uri("/failed")
        .body(
            http_body_util::Empty::<bytes::Bytes>::new()
                .map_err(|never| match never {})
                .boxed_unsync(),
        )
        .unwrap();
    let response = serve_request(app, request, "alice", &log)
        .with_subscriber(subscriber)
        .await;
    assert_eq!(response.status(), http::StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(std::fs::read_to_string(access_path).unwrap(), "existing\n");
    let errors = std::fs::read_to_string(error_path).unwrap();
    assert!(errors.contains("request failed"));
    assert!(errors.contains("status=500"));
    assert!(errors.contains("cannot write access.log"));
    assert_eq!(errors.matches("identity=\"alice\"").count(), 2);
}
