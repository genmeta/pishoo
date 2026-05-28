#[test]
fn main_uses_shared_root_cert_store() {
    let main_source = include_str!("../src/main.rs");
    assert!(
        main_source.contains("pishoo::tls::root_cert_store()"),
        "main must use the shared root cert entry"
    );
}

#[test]
fn acquire_listener_uses_dhttp_dns_publisher() {
    let state_source = include_str!("../src/hypervisor/state.rs");
    assert!(
        state_source.contains("publish_task"),
        "ListenerResource keeps a publish_task slot for the DnsPublisher migration"
    );
    let server_ops_source = include_str!("../src/hypervisor/state/server_ops.rs");
    assert!(
        server_ops_source.contains("publisher_with_options"),
        "acquire_listener must publish through dhttp Endpoint publisher"
    );
    assert!(
        !server_ops_source.contains("spawn_server_publish_task("),
        "acquire_listener must not use the legacy BindUri-based DNS publisher"
    );
}

#[test]
fn main_force_kills_lingering_workers_during_shutdown() {
    let shutdown_source = include_str!("../src/hypervisor/shutdown.rs");
    assert!(
        shutdown_source.contains("WorkerProcessError::ShutdownTimeout")
            && shutdown_source.contains("state.force_kill_workers(&shutdown_timeout)"),
        "root shutdown must SIGKILL lingering workers after graceful timeout"
    );
    let main_source = include_str!("../src/main.rs");
    assert!(
        main_source.contains("let _ = monitor_handle.await;"),
        "root shutdown must await aborted background tasks"
    );
}

#[test]
fn worker_startup_is_scoped_and_timeout_bound() {
    let spawn_source = include_str!("../src/hypervisor/process/spawn.rs");
    assert!(
        spawn_source.contains("WORKER_STARTUP_TIMEOUT")
            && spawn_source.contains("std::time::Duration::from_secs(30)"),
        "worker startup handshake must have a 30 second timeout"
    );
    assert!(
        spawn_source.contains(".spawn_worker_task(pid")
            && spawn_source.contains("start_worker_ipc("),
        "worker startup/remoc setup must run inside the worker task scope"
    );
    assert!(
        spawn_source.contains("Ok(SpawnedWorker { pid })"),
        "spawn_worker should return after scheduling startup instead of waiting for worker hello"
    );
}

#[test]
fn ipc_disconnect_uses_typed_worker_failure() {
    let spawn_source = include_str!("../src/hypervisor/process/spawn.rs");
    assert!(
        spawn_source.contains("WorkerProcessError::IpcDisconnected")
            && spawn_source.contains("state.fail_worker(pid, error).await"),
        "root/worker IPC disconnect must enter the typed worker failure path"
    );
}

#[test]
fn main_reload_uses_worker_diff() {
    let orchestrate_source = include_str!("../src/hypervisor/reload/orchestrate.rs");
    assert!(
        orchestrate_source.contains("compute_worker_diff"),
        "reload should use diff-based worker management instead of whole-set replacement"
    );
    assert!(
        orchestrate_source.contains("missing_unchanged_workers"),
        "reload should respawn desired workers that are unchanged in config but not running"
    );
}

#[test]
fn root_state_exposes_explicit_listener_operations() {
    let source = include_str!("../src/hypervisor/state/server_ops.rs");

    assert!(source.contains("pub async fn acquire_listener"));
    assert!(source.contains("pub async fn release_listener"));
    assert!(source.contains("pub async fn rebuild_listener"));
    assert!(source.contains("pub async fn clear_listener_poison"));
    assert!(!source.contains("pub async fn release_server"));
}

#[test]
fn registered_endpoint_drop_does_not_release_registry() {
    let source = include_str!("../src/listen.rs");
    let drop_impl = source
        .split("impl Drop for RegisteredEndpoint")
        .nth(1)
        .expect("RegisteredEndpoint should implement Drop");

    assert!(
        !drop_impl.contains("release_listener") && !drop_impl.contains("release_server"),
        "RegisteredEndpoint::Drop must not mutate the root listener registry"
    );
}
