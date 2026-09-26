#[tokio::test]
async fn replacing_an_unpolled_body_reaps_only_its_producer() {
    let directory = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let lib = load(STREAM, directory.path(), cancel.clone());
    let slots = Arc::new(Semaphore::new(4));
    let tasks = TaskTracker::new();
    let response = invoke(lib.clone(), &slots, &tasks)
        .await
        .execute(request("/cancel", empty()))
        .await
        .unwrap();
    let mut other = invoke(lib, &slots, &tasks)
        .await
        .execute(request("/cancel", empty()))
        .await
        .unwrap();
    let replacement = response.map(|body| {
        drop(body);
        Full::new(Bytes::from_static(b"replacement"))
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while slots.available_permits() != 3 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!cancel.is_cancelled());
    assert!(other.body_mut().frame().await.unwrap().unwrap().is_data());
    assert_eq!(
        replacement.into_body().collect().await.unwrap().to_bytes(),
        "replacement"
    );
    drop(other);
    reaped(&tasks, &slots).await;
}

#[tokio::test]
async fn dropping_pending_execute_releases_upload_and_permit() {
    let directory = tempfile::tempdir().unwrap();
    let lib = load(READ, directory.path(), CancellationToken::new());
    let slots = Arc::new(Semaphore::new(4));
    let tasks = TaskTracker::new();
    let invocation = invoke(lib, &slots, &tasks).await;
    let (tx, body) = upload();
    let execute = tokio::spawn(invocation.execute(request("/read", body)));
    tokio::task::yield_now().await;
    execute.abort();
    assert!(execute.await.unwrap_err().is_cancelled());
    reaped(&tasks, &slots).await;
    assert!(tx.is_closed());
}

#[tokio::test(start_paused = true)]
async fn unpolled_response_has_an_independent_deadline() {
    let directory = tempfile::tempdir().unwrap();
    let lib = load(STREAM, directory.path(), CancellationToken::new());
    let slots = Arc::new(Semaphore::new(4));
    let tasks = TaskTracker::new();
    let mut response = invoke(lib, &slots, &tasks)
        .await
        .execute(request("/cancel", empty()))
        .await
        .unwrap();
    assert_eq!(slots.available_permits(), 3);
    tokio::time::advance(Duration::from_secs(31)).await;
    reaped(&tasks, &slots).await;
    let error = response.body_mut().frame().await.unwrap().unwrap_err();
    assert!(matches!(
        error.downcast_ref::<Error>(),
        Some(Error::Deadline)
    ));
    assert!(response.body_mut().frame().await.is_none());
}

#[tokio::test]
async fn four_slots_survive_version_replacement_and_cancel_together() {
    let directory = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let old = load(STREAM, directory.path(), cancel.clone());
    let new = Arc::new(
        Lib::load(
            old.runtime.clone(),
            "test".into(),
            &component(STREAM),
            directory.path(),
            LibPolicy::default(),
            cancel.clone(),
        )
        .unwrap(),
    );
    let slots = Arc::new(Semaphore::new(4));
    let tasks = TaskTracker::new();
    let mut responses = Vec::new();
    for lib in [old.clone(), new.clone(), old, new.clone()] {
        responses.push(
            invoke(lib, &slots, &tasks)
                .await
                .execute(request("/cancel", empty()))
                .await
                .unwrap(),
        );
    }
    assert_eq!(slots.available_permits(), 0);
    assert!(slots.clone().try_acquire_owned().is_err());
    cancel.cancel();
    reaped(&tasks, &slots).await;
    for mut response in responses {
        assert!(response.body_mut().frame().await.unwrap().is_err());
    }
    let result = Invocation::new(
        new,
        slots.try_acquire_owned().unwrap(),
        dhttp::Endpoint::load("alice").await.unwrap(),
        &dhttp::HandshakeSummary {
            alpn: None,
            local: Some(authority("alice.dhttp.net")),
            remote: None,
        },
        TaskTracker::new(),
    );
    assert!(matches!(result, Err(Error::Cancelled)));
}

#[tokio::test]
async fn guest_join_failure_is_reported_once_and_cancels_producer() {
    let cancel = CancellationToken::new();
    let guest = tokio::spawn(async {
        panic!("guest task panic");
        #[allow(unreachable_code)]
        Ok(())
    });
    let mut body = LibResponseBody::Reading {
        inner: Empty::<Bytes>::new()
            .map_err(|never| match never {})
            .boxed_unsync(),
        guest: Some(guest),
        cancel_on_drop: cancel.clone().drop_guard(),
    };
    assert!(matches!(body.frame().await.unwrap(), Err(Error::Task(_))));
    assert!(body.frame().await.is_none());
    assert!(cancel.is_cancelled());
}

#[tokio::test]
async fn cpu_only_guest_yields_for_cancellation_and_is_reaped() {
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
    let cancel = CancellationToken::new();
    let lib = load(&bytes, directory.path(), cancel.clone());
    let slots = Arc::new(Semaphore::new(4));
    let tasks = TaskTracker::new();
    let invocation = invoke(lib.clone(), &slots, &tasks).await;
    let cancelling = async {
        tokio::time::sleep(Duration::from_millis(1)).await;
        cancel.cancel();
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(invocation.execute(request("/read", empty())), cancelling)
    })
    .await
    .unwrap();
    assert!(matches!(result, Err(Error::Cancelled)));
    reaped(&tasks, &slots).await;
}
