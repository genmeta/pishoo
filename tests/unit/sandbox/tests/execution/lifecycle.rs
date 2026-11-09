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

const PROBE: &[u8] = include_bytes!("../../../../fixtures/pishoo-stream-probe.wasm");

#[tokio::test]
async fn unpolled_standard_stream_drop_reaps_guest_waiting_on_timer() {
    let directory = tempfile::tempdir().unwrap();
    let tasks = TaskTracker::new();
    let lib = load(PROBE, directory.path());
    let response = invoke(lib, &tasks)
        .await
        .execute(request("/idle", empty()))
        .await
        .unwrap();
    assert_eq!(tasks.len(), 1);
    drop(response);
    reaped(&tasks).await;
}

#[tokio::test]
async fn retiring_version_cancels_all_its_http_calls_and_preserves_other_versions() {
    let directory = tempfile::tempdir().unwrap();
    let other_directory = tempfile::tempdir().unwrap();
    let lib = load(PROBE, directory.path());
    let other = load(PROBE, other_directory.path());
    let old_tasks = TaskTracker::new();
    let new_tasks = TaskTracker::new();
    let idle = invoke(lib.clone(), &old_tasks)
        .await
        .execute(request("/idle", empty()))
        .await
        .unwrap();
    let finite = invoke(lib.clone(), &old_tasks)
        .await
        .execute(request("/text?count=100", empty()))
        .await
        .unwrap();
    let mut current = invoke(other, &new_tasks)
        .await
        .execute(request("/text", empty()))
        .await
        .unwrap();
    lib.stream_retired.cancel();
    reaped(&old_tasks).await;
    assert_eq!(new_tasks.len(), 1);
    assert!(current.body_mut().frame().await.unwrap().unwrap().is_data());
    drop((idle, finite, current));
    reaped(&new_tasks).await;
}

#[tokio::test]
async fn response_eof_reaps_guest_even_when_body_is_retained() {
    let directory = tempfile::tempdir().unwrap();
    let tasks = TaskTracker::new();
    let lib = load(PROBE, directory.path());
    let mut response = invoke(lib, &tasks)
        .await
        .execute(request("/eof", empty()))
        .await
        .unwrap();
    let mut bytes = Vec::new();
    while let Some(frame) = response.body_mut().frame().await {
        bytes.extend_from_slice(&frame.unwrap().into_data().unwrap());
    }
    assert_eq!(bytes, b"ready\n");
    reaped(&tasks).await;
    drop(response);
}

#[tokio::test]
async fn trap_after_headers_ends_body_and_releases_store() {
    let directory = tempfile::tempdir().unwrap();
    let tasks = TaskTracker::new();
    let lib = load(PROBE, directory.path());
    let response = invoke(lib, &tasks)
        .await
        .execute(request("/trap", empty()))
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), response.into_body().collect())
        .await
        .unwrap();
    if let Ok(body) = result {
        assert_eq!(body.to_bytes(), "ready\n");
    }
    reaped(&tasks).await;
}

#[tokio::test]
async fn sandbox_close_retires_standard_http_streams() {
    let directory = tempfile::tempdir().unwrap();
    let lib = load(PROBE, directory.path());
    let mut sandbox = Sandbox::new(lib.runtime.clone());
    sandbox.libs.insert("test".into(), lib.clone());
    let response = invoke(lib, &sandbox.tasks)
        .await
        .execute(request("/idle", empty()))
        .await
        .unwrap();
    sandbox.close();
    sandbox.wait().await.unwrap();
    drop(response);
}
