#[test]
fn main_does_not_build_custom_server_quic_config() {
    let main_source = include_str!("../src/main.rs");
    assert!(
        !main_source.contains("default_server_quic_config()"),
        "main should let dhttp Endpoint construction own server defaults"
    );
    assert!(
        !main_source.contains("server_qcfg"),
        "main should not thread server config through RootState"
    );
    assert!(
        !main_source.contains("pishoo::tls::root_cert_store()"),
        "main must use the DHTTP_ROOT_CA from dhttp defaults"
    );
    assert!(
        !main_source.contains("WebPkiClientVerifier"),
        "main must not locally rebuild the DHTTP client certificate verifier"
    );
    assert!(
        !main_source.contains("alpns: vec![b\"h3\".to_vec()]"),
        "main must not hard-code DHTTP ALPN instead of inheriting dhttp defaults"
    );
}

#[test]
fn root_state_does_not_store_server_quic_config() {
    let state_source = include_str!("../src/hypervisor/state.rs");
    assert!(
        !state_source.contains("server_qcfg"),
        "RootState should not store fixed DHTTP server defaults"
    );
    assert!(
        !state_source.contains("ServerQuicConfig"),
        "RootState should not depend on low-level server QUIC config"
    );
}

#[test]
fn endpoint_factory_uses_dhttp_endpoint_for_dns_resolver() {
    let source = include_str!("../src/hypervisor/endpoint_factory.rs");
    let quic_endpoint_builder = ["Quic", "Endpoint::builder()"].concat();
    let default_client_config = ["default_", "client_quic_config()"].concat();
    let default_server_config = ["default_", "server_quic_config()"].concat();

    assert!(source.contains("Endpoint::builder()"));
    assert!(source.contains(".dns(DnsScheme::System)"));
    assert!(!source.contains(&quic_endpoint_builder));
    assert!(!source.contains(&default_client_config));
    assert!(!source.contains(&default_server_config));
}

#[test]
fn pishoo_does_not_define_custom_root_ca_bootstrap() {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let build_script = manifest_dir.join("build.rs");
    let tls_source = std::fs::read_to_string(manifest_dir.join("src/tls.rs"))
        .expect("pishoo tls source should be readable");

    assert!(!build_script.exists());
    assert!(!tls_source.contains("root_cert_store"));
    assert!(!tls_source.contains("OUT_DIR"));
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
fn registered_endpoint_drop_releases_through_guarded_transition() {
    let source = include_str!("../src/listen.rs");
    let drop_impl = source
        .split("impl Drop for RegisteredEndpoint")
        .nth(1)
        .expect("RegisteredEndpoint should implement Drop");

    assert!(
        drop_impl.contains("release_guard.take()")
            && drop_impl.contains("release_listener_for_dropped_handle"),
        "RegisteredEndpoint::Drop must schedule guarded async listener release"
    );
}

#[test]
fn monitor_does_not_restart_failed_workers() {
    let source = include_str!("../src/hypervisor/process/monitor.rs");

    assert!(!source.contains("spawn_worker(&worker_bin"));
    assert!(!source.contains("failed to restart worker"));
}

#[test]
fn reload_retries_failed_workers() {
    let source = include_str!("../src/hypervisor/reload/orchestrate.rs");

    assert!(
        source.contains("failed_desired_workers"),
        "root reload must include failed desired workers in the spawn set"
    );
}

#[test]
fn local_service_uses_worker_runtime_path() {
    let source = include_str!("../src/hypervisor/local_service.rs");
    assert!(source.contains("RuntimeRegistry::new("));
    assert!(!source.contains("setup_service("));
    assert!(!source.contains("run_service("));
}

#[test]
fn service_module_no_longer_exposes_batch_lifecycle() {
    let source = include_str!("../src/service.rs");
    assert!(!source.contains("pub async fn setup_service"));
    assert!(!source.contains("pub async fn run_service"));
    assert!(!source.contains("pub async fn collect_reusable_listeners"));
    assert!(!source.contains("pub struct PreparedServer"));
    assert!(!source.contains("pub struct ServiceConfig"));
}
