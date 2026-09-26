#[tokio::test]
async fn upload_and_response_stream_with_repeated_trailers() {
    let directory = tempfile::tempdir().unwrap();
    let lib = load(READ, directory.path(), CancellationToken::new());
    let slots = Arc::new(Semaphore::new(4));
    let tasks = TaskTracker::new();
    let invocation = invoke(lib, &slots, &tasks).await;
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
    reaped(&tasks, &slots).await;
}

#[tokio::test]
async fn response_headers_arrive_before_upload_eof() {
    let directory = tempfile::tempdir().unwrap();
    let lib = load(EARLY, directory.path(), CancellationToken::new());
    let slots = Arc::new(Semaphore::new(4));
    let tasks = TaskTracker::new();
    let invocation = invoke(lib, &slots, &tasks).await;
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
    assert_eq!(slots.available_permits(), 3);
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
    reaped(&tasks, &slots).await;
}

#[tokio::test]
async fn upload_error_becomes_one_body_error_and_releases_permit() {
    let directory = tempfile::tempdir().unwrap();
    let lib = load(EARLY, directory.path(), CancellationToken::new());
    let slots = Arc::new(Semaphore::new(4));
    let tasks = TaskTracker::new();
    let (tx, body) = upload();
    let mut response = invoke(lib, &slots, &tasks)
        .await
        .execute(request("/early", body))
        .await
        .unwrap();
    tx.send(Err(std::io::Error::other("upload reset").into()))
        .await
        .unwrap();
    drop(tx);
    loop {
        if response.body_mut().frame().await.unwrap().is_err() {
            break;
        }
    }
    assert!(response.body_mut().frame().await.is_none());
    reaped(&tasks, &slots).await;
}

#[tokio::test]
async fn response_body_waits_for_guest_before_final_trailers_and_disarms_on_eof() {
    let cancel = CancellationToken::new();
    let (tx, rx) = oneshot::channel();
    let guest = tokio::spawn(async {
        rx.await.unwrap();
        Ok(())
    });
    let mut trailers = http::HeaderMap::new();
    trailers.append("x-end", "first".parse().unwrap());
    trailers.append("x-end", "second".parse().unwrap());
    let frames = futures::stream::iter([
        Ok(Frame::data(Bytes::from_static(b"data"))),
        Ok(Frame::trailers(trailers)),
    ]);
    let mut body = LibResponseBody::Reading {
        inner: StreamBody::new(frames).boxed_unsync(),
        guest: Some(guest),
        cancel_on_drop: cancel.clone().drop_guard(),
    };
    assert_eq!(
        body.frame().await.unwrap().unwrap().into_data().unwrap(),
        "data"
    );
    assert!(futures::poll!(body.frame()).is_pending());
    assert!(matches!(body, LibResponseBody::Waiting { .. }));
    tx.send(()).unwrap();
    assert_eq!(
        body.frame()
            .await
            .unwrap()
            .unwrap()
            .into_trailers()
            .unwrap()
            .get_all("x-end")
            .iter()
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
    assert!(body.frame().await.is_none());
    drop(body);
    assert!(!cancel.is_cancelled());
}
