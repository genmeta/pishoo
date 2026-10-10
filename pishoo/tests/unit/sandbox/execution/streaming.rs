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

// Diagnose the guest result after an early response. Production intentionally
// detaches that result, so the network caller only observes the body/reset error.
#[tokio::test]
#[ignore = "explicit 100 MiB Echo/fuel diagnostic; independent of transport acceptance"]
async fn large_echo_reports_fuel_exhaustion() {
    use wasmtime::Store;
    use wasmtime_wasi_http::{
        WasiHttpCtx,
        p2::{
            WasiHttpView,
            bindings::{Proxy, http::types},
        },
    };

    let directory = tempfile::tempdir().unwrap();
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let lib = Lib::load(
        runtime,
        "echo".into(),
        include_bytes!("../../../../assets/tcp-demo-echo.wasm"),
        directory.path(),
    )
    .unwrap();
    let mut wasi = wasmtime_wasi::WasiCtx::builder().build();
    *wasi.filesystem() = lib.filesystem.clone();
    let mut store = Store::new(
        &lib.runtime.engine,
        StoreData {
            table: wasmtime::component::ResourceTable::new(),
            wasi,
            http: WasiHttpCtx::new(),
            memory: StoreLimits::default(),
            deny_outgoing: DenyOutgoing,
            local: authority("alice.dhttp.net"),
            remote: None,
        },
    );
    store.limiter(|data| &mut data.memory);
    store.set_fuel(100_000_000).unwrap();
    store.fuel_async_yield_interval(Some(10_000)).unwrap();
    let proxy = Proxy::instantiate_async(&mut store, &lib.component, &lib.runtime.linker)
        .await
        .unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let incoming = futures::stream::iter(
        (0..1600).map(|_| Ok::<_, types::ErrorCode>(Frame::data(Bytes::from(vec![0x5a; 65536])))),
    );
    let incoming = Request::post("https://alice.dhttp.net/echo")
        .body(StreamBody::new(incoming))
        .unwrap();
    let incoming = store
        .data_mut()
        .http()
        .new_incoming_request(types::Scheme::Https, incoming)
        .unwrap();
    let out = store.data_mut().http().new_response_outparam(tx).unwrap();
    let read = async {
        let mut response = rx.await.unwrap().unwrap();
        let mut received = 0;
        while let Some(frame) = response.body_mut().frame().await {
            match frame {
                Ok(frame) => {
                    if let Ok(data) = frame.into_data() {
                        received += data.len();
                    }
                }
                Err(_) => break,
            }
        }
        received
    };
    let guest = async move {
        let result = proxy
            .wasi_http_incoming_handler()
            .call_handle(&mut store, incoming, out)
            .await;
        let remaining = store.get_fuel().unwrap();
        drop(store);
        (result, remaining)
    };
    let ((guest, remaining), received) =
        tokio::time::timeout(Duration::from_secs(60), async { tokio::join!(guest, read) })
            .await
            .unwrap();
    let error = guest.unwrap_err();
    println!(
        "GUEST_DIAGNOSTIC bytes={received} remaining_fuel={} error={error:#}",
        remaining
    );
    assert_eq!(
        error.downcast_ref::<wasmtime::Trap>(),
        Some(&wasmtime::Trap::OutOfFuel)
    );
    assert!(received > 0 && received < 100 * 1024 * 1024);
}
