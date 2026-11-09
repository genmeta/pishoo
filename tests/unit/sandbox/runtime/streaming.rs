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

#[tokio::test]
async fn epoch_refills_http_but_not_cli_after_idle() {
    let runtime = WasmRuntime::new().unwrap();
    let module = wasmtime::Module::new(
        &runtime.engine,
        r#"(module
            (func (export "work") (param $n i32)
                (loop $l local.get $n i32.const 1 i32.sub local.tee $n br_if $l)))"#,
    )
    .unwrap();
    for replenish in [false, true] {
        let mut store = Store::new(&runtime.engine, ());
        configure_execution(&mut store, replenish).unwrap();
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
        assert_eq!(result.is_ok(), replenish);
    }
}

#[tokio::test]
async fn cpu_only_http_still_exhausts_its_budget() {
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

#[test]
fn experimental_events_import_is_rejected_at_link_validation() {
    let runtime = WasmRuntime::new().unwrap();
    let bytes = wat::parse_str(
        r#"(component
        (import "pishoo:events/change@0.1.0" (instance
            (export "current" (func (result u64))))))"#,
    )
    .unwrap();
    assert!(
        matches!(runtime.compile(&bytes), Err(Error::InvalidComponent(message)) if message.contains("pishoo:events/change"))
    );
}
