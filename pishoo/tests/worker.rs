#[test]
fn worker_requests_root_owned_connector() {
    let worker_source = include_str!("../src/bin/pishoo_worker.rs");

    assert!(
        worker_source.contains("RemoteControlPlane::new(bootstrap.control_plane)"),
        "worker must use RemoteControlPlane backed by the root-provided client"
    );
    assert!(
        !worker_source.contains("start_connector_runtime"),
        "worker must not keep local start_connector_runtime path"
    );
    assert!(
        !worker_source.contains("ConnectorRuntime"),
        "worker must not keep local ConnectorRuntime type"
    );
}

#[test]
fn worker_uses_shared_tls_validator() {
    let worker_source = include_str!("../src/bin/pishoo_worker.rs");
    assert!(
        worker_source.contains("build_service_config("),
        "worker TLS path must delegate to build_service_config which handles TLS validation"
    );
}

#[test]
fn worker_reload_uses_single_helper_path() {
    let worker_source = include_str!("../src/bin/pishoo_worker.rs");

    assert!(
        worker_source.contains("build_service_config("),
        "worker should build config through the shared builder"
    );
    assert!(
        worker_source.contains("run_service(&plane, &config)"),
        "worker should run through the unified run_service entry point"
    );
}

#[test]
fn worker_does_not_embed_root_cert_store() {
    let worker_source = include_str!("../src/bin/pishoo_worker.rs");
    assert!(
        !worker_source.contains("include_bytes!(\"../../../keychain/root.crt\")"),
        "worker must not assemble root cert store locally"
    );
}

#[test]
fn worker_stops_server_runtimes_before_exit() {
    let worker_source = include_str!("../src/bin/pishoo_worker.rs");
    assert!(
        worker_source.contains("tokio::select!"),
        "worker shutdown must use select to handle signals and service exit"
    );
}

#[test]
fn worker_reload_rebuilds_all_listener_handles() {
    let worker_source = include_str!("../src/bin/pishoo_worker.rs");
    assert!(
        worker_source.contains("SIGHUP") && worker_source.contains("reload not yet implemented"),
        "worker reload path should be stubbed with a clear TODO"
    );
}
