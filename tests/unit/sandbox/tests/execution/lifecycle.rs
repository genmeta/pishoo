use super::*;

#[tokio::test]
async fn replacing_an_unpolled_body_keeps_other_execution_running() {
    let directory = tempfile::tempdir().unwrap();
    let lib = load(STREAM, directory.path());
    let tasks = TaskTracker::new();
    let response = invoke(lib.clone(), &tasks)
        .await
        .execute(request("/cancel", empty()))
        .await
        .unwrap();
    let mut other = invoke(lib, &tasks)
        .await
        .execute(request("/cancel", empty()))
        .await
        .unwrap();
    let replacement = response.map(|body| {
        drop(body);
        Full::new(Bytes::from_static(b"replacement"))
    });
    assert!(other.body_mut().frame().await.unwrap().unwrap().is_data());
    assert_eq!(
        replacement.into_body().collect().await.unwrap().to_bytes(),
        "replacement"
    );
    drop(other);
}

#[tokio::test]
async fn dropping_pending_execute_reaps_guest_without_upload_eof() {
    let directory = tempfile::tempdir().unwrap();
    let lib = load(READ, directory.path());
    let tasks = TaskTracker::new();
    let invocation = invoke(lib, &tasks).await;
    let (tx, body) = upload();
    let execute = tokio::spawn(invocation.execute(request("/read", body)));
    tokio::task::yield_now().await;
    execute.abort();
    assert!(execute.await.unwrap_err().is_cancelled());
    reaped(&tasks).await;
    assert!(tx.is_closed());
}

#[tokio::test(start_paused = true)]
async fn unpolled_response_survives_thirty_seconds() {
    let directory = tempfile::tempdir().unwrap();
    let lib = load(STREAM, directory.path());
    let tasks = TaskTracker::new();
    let response = invoke(lib, &tasks)
        .await
        .execute(request("/cancel", empty()))
        .await
        .unwrap();
    assert_eq!(tasks.len(), 1);
    tokio::time::advance(Duration::from_secs(31)).await;
    assert_eq!(tasks.len(), 1);
    drop(response);
}

#[tokio::test]
async fn cpu_only_guest_exhausts_fuel() {
    let bytes = wat::parse_str(
        r#"
        (component
            (import "wasi:http/types@0.2.12" (instance $types
                (export "incoming-request" (type (sub resource)))
                (export "response-outparam" (type (sub resource)))
            ))
            (alias export $types "incoming-request" (type $request))
            (alias export $types "response-outparam" (type $response))
            (core module $guest
                (func (export "handle") (param i32 i32)
                    (loop $forever br $forever)))
            (core instance $guest (instantiate $guest))
            (func $handle
                (param "request" (own $request))
                (param "response-out" (own $response))
                (canon lift (core func $guest "handle")))
            (instance $handler (export "handle" (func $handle)))
            (export "wasi:http/incoming-handler@0.2.12" (instance $handler))
        )
    "#,
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let lib = load(&bytes, directory.path());
    let tasks = TaskTracker::new();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        invoke(lib, &tasks).await.execute(request("/read", empty())),
    )
    .await
    .unwrap();
    assert!(matches!(result, Err(Error::Guest(error)) if format!("{error:#}").contains("fuel")));
    reaped(&tasks).await;
}

#[tokio::test]
async fn unpolled_stream_body_drop_reaps_guest() {
    let directory = tempfile::tempdir().unwrap();
    let mut lib = load(
        include_bytes!("../../../../fixtures/pishoo-stream-probe.wasm"),
        directory.path(),
    );
    Arc::get_mut(&mut lib).unwrap().id = "std.dhttp.net/message".into();
    let tasks = TaskTracker::new();
    let mut req = request("/message/list", empty());
    *req.method_mut() = http::Method::GET;
    req.headers_mut()
        .insert(http::header::ACCEPT, "text/event-stream".parse().unwrap());
    let response = invoke(lib, &tasks).await.execute(req).await.unwrap();
    assert_eq!(tasks.len(), 1);
    drop(response);
    reaped(&tasks).await;
}

#[tokio::test]
async fn retiring_version_cancels_only_its_streams() {
    let directory = tempfile::tempdir().unwrap();
    let mut lib = load(
        include_bytes!("../../../../fixtures/pishoo-stream-probe.wasm"),
        directory.path(),
    );
    Arc::get_mut(&mut lib).unwrap().id = "std.dhttp.net/message".into();
    let tasks = TaskTracker::new();
    let mut req = request("/message/list", empty());
    *req.method_mut() = http::Method::GET;
    req.headers_mut()
        .insert(http::header::ACCEPT, "text/event-stream".parse().unwrap());
    let stream = invoke(lib.clone(), &tasks)
        .await
        .execute(req)
        .await
        .unwrap();
    let ordinary = invoke(lib.clone(), &tasks)
        .await
        .execute(request("/cancel", empty()))
        .await
        .unwrap();
    lib.stream_retired.cancel();
    tokio::time::timeout(Duration::from_secs(5), async {
        while tasks.len() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(stream);
    assert_eq!(tasks.len(), 1);
    drop(ordinary);
    reaped(&tasks).await;
}

#[tokio::test]
async fn notifications_wake_only_the_same_lib_and_identity() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let bytes = include_bytes!("../../../../fixtures/pishoo-stream-probe.wasm");
    let lib = load(bytes, first.path());
    let other_lib = load(bytes, second.path());
    let tasks = TaskTracker::new();
    let mut response = invoke(lib.clone(), &tasks)
        .await
        .execute(request("/events", empty()))
        .await
        .unwrap();
    assert_eq!(
        response
            .body_mut()
            .frame()
            .await
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap(),
        "ready\n"
    );
    invoke(other_lib, &tasks)
        .await
        .execute(request("/notify", empty()))
        .await
        .unwrap()
        .into_body()
        .collect()
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), response.body_mut().frame())
            .await
            .is_err()
    );
    invoke(lib, &tasks)
        .await
        .execute(request("/notify", empty()))
        .await
        .unwrap()
        .into_body()
        .collect()
        .await
        .unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(2), response.body_mut().frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(frame.into_data().unwrap(), "changed\n");
    drop(response);
    reaped(&tasks).await;
}
