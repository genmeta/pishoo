use http_body::Frame;
use http_body_util::StreamBody;

use super::*;

#[tokio::test]
async fn fragmented_memory_body_executes_and_returns_json() {
    let cwd = tempfile::tempdir().unwrap();
    let input = br#"{"program":"/bin/echo","args":["memory-stream"],"stdin_base64":""}"#;
    let frames = futures::stream::iter([
        Ok::<_, dhttp::BoxError>(Frame::data(Bytes::copy_from_slice(&input[..17]))),
        Ok(Frame::data(Bytes::copy_from_slice(&input[17..]))),
    ]);
    let request = Request::builder()
        .method(Method::POST)
        .uri("/exec")
        .header(header::CONTENT_TYPE, "application/json")
        .body(StreamBody::new(frames).boxed_unsync())
        .unwrap();
    let tasks = TaskTracker::new();
    let response = execute_authorized(cwd.path(), tasks.clone(), request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let result: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(result["exit_code"], 0);
    assert_eq!(
        STANDARD
            .decode(result["stdout_base64"].as_str().unwrap())
            .unwrap(),
        b"memory-stream\n"
    );
    assert!(tasks.is_empty());
}

#[tokio::test]
async fn closed_task_tracker_rejects_a_pending_memory_body() {
    let cwd = tempfile::tempdir().unwrap();
    let frames = futures::stream::pending::<std::result::Result<Frame<Bytes>, dhttp::BoxError>>();
    let request = Request::builder()
        .method(Method::POST)
        .uri("/exec")
        .header(header::CONTENT_TYPE, "application/json")
        .body(StreamBody::new(frames).boxed_unsync())
        .unwrap();
    let tasks = TaskTracker::new();
    tasks.close();
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        execute_authorized(cwd.path(), tasks, request),
    )
    .await
    .unwrap();
    assert!(matches!(result, Err(Error::Closed)));
}

#[tokio::test]
async fn more_than_four_exec_requests_run_concurrently() {
    let cwd = tempfile::tempdir().unwrap();
    let tasks = TaskTracker::new();
    let executions = (0..5).map(|_| {
        let request = Request::builder()
            .method(Method::POST)
            .uri("/exec")
            .header(header::CONTENT_TYPE, "application/json")
            .body(
                Full::new(Bytes::from_static(
                    br#"{"program":"/bin/sleep","args":["1"]}"#,
                ))
                .map_err(|error| match error {})
                .boxed_unsync(),
            )
            .unwrap();
        execute_authorized(cwd.path(), tasks.clone(), request)
    });
    for result in futures::future::join_all(executions).await {
        assert_eq!(result.unwrap().status(), StatusCode::OK);
    }
    assert!(tasks.is_empty());
}

#[tokio::test]
async fn exec_collects_output_and_exit_status() {
    let mut command = Command::new("/bin/echo");
    command
        .arg("hello")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let child = command.spawn().unwrap();
    let (status, stdout, stderr) = run_command(child, Vec::new(), CancellationToken::new())
        .await
        .unwrap();
    assert!(status.success());
    assert_eq!(stdout, b"hello\n");
    assert!(stderr.is_empty());
}

#[tokio::test]
async fn cancelled_exec_reaps_its_child() {
    let mut command = Command::new("/bin/sleep");
    command
        .arg("10")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let child = command.spawn().unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        run_command(child, Vec::new(), cancel),
    )
    .await
    .unwrap();
    assert!(matches!(result, Err(Error::Cancelled)));
}

#[tokio::test]
async fn output_limit_stops_an_unbounded_command() {
    let mut command = Command::new("/usr/bin/yes");
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let child = command.spawn().unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        run_command(child, Vec::new(), CancellationToken::new()),
    )
    .await
    .unwrap();
    assert!(matches!(result, Err(Error::BadRequest(_))));
}
