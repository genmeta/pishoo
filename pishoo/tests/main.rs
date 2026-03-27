#[test]
fn main_uses_shared_root_cert_store() {
    let main_source = include_str!("../src/main.rs");
    assert!(
        main_source.contains("pishoo::tls::root_cert_store()"),
        "main must use the shared root cert entry"
    );
}

#[test]
fn main_primes_dns_publish_before_background_task() {
    let main_source = include_str!("../src/main.rs");
    assert!(
        main_source.contains("gateway::dns::publish_now(&listeners, &publish_configs).await;"),
        "root reload/startup must publish dns records before swapping publisher handles"
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
fn main_waits_for_listener_reregistration_before_reload_republish() {
    let main_source = include_str!("../src/main.rs");
    assert!(
        main_source.contains("wait_for_reload_servers(&listeners, &publish_names).await;"),
        "reload should wait for listener re-registration before DNS republish"
    );
}
