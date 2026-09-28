use super::*;

#[tokio::test]
async fn upload_and_response_stream_with_repeated_trailers() {
    let directory = tempfile::tempdir().unwrap();
    let lib = load(READ, directory.path());
    let tasks = TaskTracker::new();
    let invocation = invoke(lib, &tasks).await;
    let (tx, body) = upload();
    let producer = async move {
        tx.send(Ok(Frame::data(Bytes::from_static(
            b"hello-hello-hello-hello-",
        ))))
        .await
        .unwrap();
        tx.send(Ok(Frame::data(Bytes::from_static(
            b"hello-hello-hello-hello",
        ))))
        .await
        .unwrap();
        let mut trailers = http::HeaderMap::new();
        trailers.append("x-request-trailer", "preserved".parse().unwrap());
        tx.send(Ok(Frame::trailers(trailers))).await.unwrap();
    };
    let (response, ()) = tokio::join!(invocation.execute(request("/read", body)), producer);
    let response = response.unwrap();
    assert_eq!(response.status(), http::StatusCode::CREATED);
    let collected = response.into_body().collect().await.unwrap();
    assert_eq!(
        collected
            .trailers()
            .unwrap()
            .get_all("x-response-trailer")
            .iter()
            .collect::<Vec<_>>(),
        ["preserved", "also-preserved"]
    );
    assert_eq!(collected.to_bytes(), b"world-world-".repeat(64));
    reaped(&tasks).await;
}

#[tokio::test]
async fn response_headers_arrive_before_upload_eof() {
    let directory = tempfile::tempdir().unwrap();
    let lib = load(EARLY, directory.path());
    let tasks = TaskTracker::new();
    let invocation = invoke(lib, &tasks).await;
    let (tx, body) = upload();
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        invocation.execute(request("/early", body)),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        response.headers()["x-handler-mode"],
        "respond-then-read-request"
    );
    assert_eq!(tasks.len(), 1);
    let producer = async move {
        tx.send(Ok(Frame::data(Bytes::from_static(b"streamed upload"))))
            .await
            .unwrap();
        let mut trailers = http::HeaderMap::new();
        trailers.append("x-request-trailer", "preserved".parse().unwrap());
        tx.send(Ok(Frame::trailers(trailers))).await.unwrap();
    };
    let (collected, ()) = tokio::join!(response.into_body().collect(), producer);
    let collected = collected.unwrap();
    assert_eq!(
        collected.trailers().unwrap()["x-response-trailer"],
        "request-consumed"
    );
    assert_eq!(collected.to_bytes(), "streamed upload");
    reaped(&tasks).await;
}

#[tokio::test]
async fn upload_error_ends_native_body_and_reaps_guest() {
    let directory = tempfile::tempdir().unwrap();
    let lib = load(EARLY, directory.path());
    let tasks = TaskTracker::new();
    let (tx, body) = upload();
    let response = invoke(lib, &tasks)
        .await
        .execute(request("/early", body))
        .await
        .unwrap();
    tx.send(Err(std::io::Error::other("upload reset").into()))
        .await
        .unwrap();
    drop(tx);
    tokio::time::timeout(Duration::from_secs(5), response.into_body().collect())
        .await
        .unwrap()
        .unwrap();
    reaped(&tasks).await;
}
