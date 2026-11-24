use tokio::io::AsyncWriteExt;
use wasmtime::component::Component;

use super::*;
use crate::sandbox::WasmRuntime;

fn parent(runtime: &WasmRuntime, body: &str) -> StoreData {
    let program = Component::new(
        &runtime.engine,
        format!(
            r#"(component
                (core module $m (func (export "run") (result i32) {body}))
                (core instance $i (instantiate $m))
                (func $run (result (result)) (canon lift (core func $i "run")))
                (instance (export "wasi:cli/run@0.2.0") (export "run" (func $run))))"#
        ),
    )
    .unwrap();
    StoreData {
        caller_service: None,
        commands: Arc::new(std::collections::HashMap::from([("probe".into(), program)])),
        table: ResourceTable::new(),
        wasi: WasiCtx::builder().build(),
        http: WasiHttpCtx::new(),
        memory: execution_limits(),
        outgoing: Outgoing::Denied,
        local: crate::test_support::identity::endpoint("alice.dhttp.net")
            .local_authority()
            .unwrap()
            .clone(),
        remote: None,
        sqlite_locks: Default::default(),
    }
}

fn request() -> Request {
    Request {
        program: "probe".into(),
        args: vec!["probe".into()],
        env: vec![("VALUE".into(), "input".into())],
        stdin: b"input".to_vec(),
        directories: vec![],
        timeout_ms: 1000,
        fuel: 10_000_000,
    }
}

#[tokio::test]
async fn child_commands_report_success_failure_traps_and_resource_limits() {
    let runtime = WasmRuntime::new().unwrap();
    for (body, reason, code) in [
        ("i32.const 0", TerminationReason::Exited, Some(0)),
        ("i32.const 1", TerminationReason::Exited, Some(1)),
        ("unreachable", TerminationReason::Trap, None),
    ] {
        let result = execute_child(&mut parent(&runtime, body), request())
            .await
            .unwrap();
        assert_eq!(result.termination_reason, reason);
        assert_eq!(result.exit_code, code);
        assert!(result.stdout.is_empty());
        assert!(result.stderr.is_empty());
        assert!(!result.truncated);
    }
    let mut state = parent(&runtime, "(loop br 0) i32.const 0");
    let mut fuel = request();
    fuel.fuel = 100;
    let result = execute_child(&mut state, fuel).await.unwrap();
    assert_eq!(result.termination_reason, TerminationReason::FuelExhausted);
    assert_eq!(result.exit_code, None);
    let mut timeout = request();
    timeout.timeout_ms = 1;
    timeout.fuel = 10_000_000_000;
    let result = execute_child(&mut state, timeout).await.unwrap();
    assert_eq!(result.termination_reason, TerminationReason::Timeout);
    assert_eq!(result.exit_code, None);
}

#[tokio::test]
async fn child_requests_reject_invalid_names_arguments_environment_and_budgets() {
    use bindings::pishoo::execution::runtime::Host;

    let runtime = WasmRuntime::new().unwrap();
    let mut state = parent(&runtime, "i32.const 0");
    for invalid in ["", "../probe", "has space", "probe.exe", &"a".repeat(65)] {
        let mut input = request();
        input.program = invalid.into();
        assert!(matches!(
            state.execute(input).await.unwrap(),
            Err(ExecutionError::InvalidRequest)
        ));
    }
    let mut missing = request();
    missing.program = "missing".into();
    assert!(matches!(
        state.execute(missing).await.unwrap(),
        Err(ExecutionError::ProgramNotFound)
    ));
    for case in 0..12 {
        let mut input = request();
        match case {
            0 => input.args = vec![String::new(); 513],
            1 => input.args = vec!["a".repeat(2 * 1024 * 1024 + 1)],
            2 => input.args = vec!["a\0b".into()],
            3 => input.env = vec![("A".into(), "B".into()); 129],
            4 => input.env = vec![("A".into(), "b".repeat(32 * 1024))],
            5 => input.env = vec![(String::new(), "B".into())],
            6 => input.env = vec![("A=B".into(), "B".into())],
            7 => input.env = vec![("A".into(), "b\0c".into())],
            8 => input.stdin = vec![0; OUTPUT_LIMIT + 1],
            9 => input.timeout_ms = 0,
            10 => input.timeout_ms = 60_001,
            11 => input.fuel = 10_000_000_001,
            _ => unreachable!(),
        }
        assert!(
            matches!(
                execute_child(&mut state, input).await,
                Err(ExecutionError::InvalidRequest)
            ),
            "case {case}"
        );
    }
}

#[tokio::test]
async fn child_directory_grants_require_safe_paths_and_real_directory_handles() {
    use bindings::pishoo::execution::runtime::Directory;
    use wasmtime_wasi::p2::bindings::filesystem::preopens::Host;

    let runtime = WasmRuntime::new().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let mut state = parent(&runtime, "i32.const 0");
    let mut wasi = WasiCtx::builder();
    wasi.preopened_dir(folder.path(), "/db", FsPerms::ReadOnly)
        .unwrap();
    state.wasi = wasi.build();
    for path in [
        "/tmp",
        "relative",
        "/a/../b",
        "/a//b",
        "/a/./b",
        "/a\\b",
        "/a\0b",
        &format!("/{}", "a".repeat(128)),
        "/work",
    ] {
        let directories = wasmtime_wasi::filesystem::WasiFilesystemCtxView {
            ctx: state.wasi.filesystem(),
            table: &mut state.table,
        }
        .get_directories()
        .unwrap();
        let mut input = request();
        input.directories = vec![Directory {
            descriptor: directories.into_iter().next().unwrap().0,
            path: path.into(),
            read_only: false,
        }];
        let result = execute_child(&mut state, input).await;
        if path == "/work" {
            assert_eq!(result.unwrap().exit_code, Some(0));
        } else {
            assert!(
                matches!(result, Err(ExecutionError::InvalidRequest)),
                "{path}"
            );
        }
    }
}

#[tokio::test]
async fn capture_drains_both_streams_and_bounds_their_combined_output() {
    let (out_read, mut out_write) = tokio::io::duplex(16);
    let (err_read, mut err_write) = tokio::io::duplex(16);
    let (result, _, _) = tokio::join!(
        capture(out_read, err_read),
        async move {
            out_write.write_all(b"output").await.unwrap();
        },
        async move {
            err_write.write_all(b"error").await.unwrap();
        },
    );
    let (out, err, truncated) = result.unwrap();
    assert_eq!(out, b"output");
    assert_eq!(err, b"error");
    assert!(!truncated);
    let (out_read, mut out_write) = tokio::io::duplex(1024);
    let (err_read, err_write) = tokio::io::duplex(1024);
    drop(err_write);
    let (result, _) = tokio::join!(capture(out_read, err_read), async move {
        let _ = out_write.write_all(&vec![b'x'; OUTPUT_LIMIT + 1]).await;
    });
    let (out, err, truncated) = result.unwrap();
    assert_eq!(out.len() + err.len(), OUTPUT_LIMIT);
    assert!(truncated);
}
