#[test]
fn main_uses_shared_root_cert_store() {
    let main_source = include_str!("../src/main.rs");
    assert!(
        main_source.contains("pishoo::tls::root_cert_store()"),
        "main must use the shared root cert entry"
    );
}

#[test]
fn register_listener_uses_dhttp_dns_publisher() {
    let state_source = include_str!("../src/hypervisor/state.rs");
    assert!(
        state_source.contains("publish_task"),
        "ServerEntry keeps a publish_task slot for the DnsPublisher migration"
    );
    let server_ops_source = include_str!("../src/hypervisor/state/server_ops.rs");
    assert!(
        server_ops_source.contains("publisher_with_options"),
        "register_listener must publish through dhttp Endpoint publisher"
    );
    assert!(
        !server_ops_source.contains("spawn_server_publish_task("),
        "register_listener must not use the legacy BindUri-based DNS publisher"
    );
}

#[test]
fn main_force_kills_lingering_workers_during_shutdown() {
    let shutdown_source = include_str!("../src/hypervisor/shutdown.rs");
    assert!(
        shutdown_source.contains("state.force_kill_workers(\"shutdown_timeout\")"),
        "root shutdown must SIGKILL lingering workers after graceful timeout"
    );
    let main_source = include_str!("../src/main.rs");
    assert!(
        main_source.contains("let _ = monitor_handle.await;"),
        "root shutdown must await aborted background tasks"
    );
}

#[test]
fn main_reload_uses_worker_diff() {
    let orchestrate_source = include_str!("../src/hypervisor/reload/orchestrate.rs");
    assert!(
        orchestrate_source.contains("compute_worker_diff"),
        "reload should use diff-based worker management instead of whole-set replacement"
    );
}
