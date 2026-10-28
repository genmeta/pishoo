use super::*;

#[test]
fn fuel_refill_is_bounded_by_time_and_burst() {
    assert_eq!(replenished_fuel(123, Duration::ZERO), 123);
    assert_eq!(replenished_fuel(123, Duration::from_millis(100)), 1_000_123);
    assert_eq!(replenished_fuel(123, Duration::from_secs(3600)), BURST_FUEL);
    assert_eq!(
        replenished_fuel(BURST_FUEL, Duration::from_secs(1)),
        BURST_FUEL
    );
}

#[test]
fn accept_header_does_not_grant_streaming_for_wildcards_or_zero_quality() {
    assert!(accepts_events("text/event-stream"));
    assert!(accepts_events("application/json, text/event-stream;q=0.5"));
    for value in [
        "*/*",
        "application/json",
        "text/event-stream;q=0",
        "text/event-stream;q=NaN",
    ] {
        assert!(!accepts_events(value));
    }
}

#[test]
fn stream_allowlist_uses_identity_method_and_exact_guest_path() {
    assert!(stream_operation(
        "std.dhttp.net/message",
        &http::Method::GET,
        "/message/list"
    ));
    assert!(stream_operation(
        "dev.genmeta.tech.dhttp.net/message",
        &http::Method::GET,
        "/message/list"
    ));
    for id in [
        "message",
        "evil.example/message",
        "dev.genmeta.dhttp.net/message",
        "std.dhttp.net/profile",
    ] {
        assert!(!stream_operation(id, &http::Method::GET, "/message/list"));
    }
    assert!(!stream_operation(
        "std.dhttp.net/message",
        &http::Method::POST,
        "/message/list"
    ));
    assert!(!stream_operation(
        "std.dhttp.net/message",
        &http::Method::GET,
        "/message/list/"
    ));
}

#[tokio::test]
async fn epoch_refills_stream_but_not_ordinary_execution_after_idle() {
    let runtime = WasmRuntime::new().unwrap();
    let module = wasmtime::Module::new(
        &runtime.engine,
        r#"(module
            (func (export "work") (param $n i32)
                (loop $l local.get $n i32.const 1 i32.sub local.tee $n br_if $l)))"#,
    )
    .unwrap();
    for streaming in [false, true] {
        let mut store = Store::new(&runtime.engine, ());
        configure_execution(&mut store, streaming).unwrap();
        let instance = wasmtime::Instance::new_async(&mut store, &module, &[])
            .await
            .unwrap();
        let work = instance
            .get_typed_func::<i32, ()>(&mut store, "work")
            .unwrap();
        store.set_fuel(1000).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        runtime.engine.increment_epoch();
        let result = work.call_async(&mut store, 10_000).await;
        assert_eq!(result.is_ok(), streaming);
    }
}

#[tokio::test]
async fn cpu_only_stream_still_exhausts_its_budget() {
    let runtime = WasmRuntime::new().unwrap();
    let module = wasmtime::Module::new(
        &runtime.engine,
        r#"(module (func (export "run") (loop $l br $l)))"#,
    )
    .unwrap();
    let mut store = Store::new(&runtime.engine, ());
    configure_execution(&mut store, true).unwrap();
    let instance = wasmtime::Instance::new_async(&mut store, &module, &[])
        .await
        .unwrap();
    let run = instance
        .get_typed_func::<(), ()>(&mut store, "run")
        .unwrap();
    let error = tokio::time::timeout(Duration::from_secs(5), run.call_async(&mut store, ()))
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<wasmtime::Trap>(),
        Some(&wasmtime::Trap::OutOfFuel)
    );
}

#[tokio::test]
async fn explicit_limits_reject_guest_memory_growth() {
    let runtime = WasmRuntime::new().unwrap();
    let module = wasmtime::Module::new(
        &runtime.engine,
        r#"(module (memory 1) (func (export "grow") (result i32) i32.const 4096 memory.grow))"#,
    )
    .unwrap();
    let mut store = Store::new(&runtime.engine, execution_limits());
    store.limiter(|limits| limits);
    configure_execution(&mut store, true).unwrap();
    let instance = wasmtime::Instance::new_async(&mut store, &module, &[])
        .await
        .unwrap();
    assert_eq!(
        instance
            .get_typed_func::<(), i32>(&mut store, "grow")
            .unwrap()
            .call_async(&mut store, ())
            .await
            .unwrap(),
        -1
    );
}
