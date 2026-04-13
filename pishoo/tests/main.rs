#[test]
fn main_uses_shared_root_cert_store() {
    let main_source = include_str!("../src/main.rs");
    assert!(
        main_source.contains("pishoo::tls::root_cert_store()"),
        "main must use the shared root cert entry"
    );
}

#[test]
fn main_uses_reactive_per_server_dns_publish() {
    let state_source = include_str!("../src/hypervisor/state.rs");
    assert!(
        state_source.contains("publish_task"),
        "register_listener must spawn per-server DNS publish tasks via ServerEntry"
    );
}

#[test]
fn main_force_kills_lingering_workers_during_shutdown() {
    let main_source = include_str!("../src/main.rs");
    assert!(
        main_source.contains("state.force_kill_workers(\"shutdown_timeout\")"),
        "root shutdown must SIGKILL lingering workers after graceful timeout"
    );
    assert!(
        main_source.contains("let _ = accept_handle.await;")
            && main_source.contains("let _ = monitor_handle.await;"),
        "root shutdown must await aborted background tasks"
    );
}

#[test]
fn main_reload_uses_worker_diff() {
    let main_source = include_str!("../src/main.rs");
    assert!(
        main_source.contains("compute_worker_diff"),
        "reload should use diff-based worker management instead of whole-set replacement"
    );
}
