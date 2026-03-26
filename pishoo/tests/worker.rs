#[test]
fn worker_requests_root_owned_connector() {
    let worker_source = include_str!("../src/bin/pishoo_worker.rs");

    assert!(
        worker_source.contains(".open_connector(OpenConnector {"),
        "worker must request connector from root_api.open_connector"
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
        worker_source.contains("pishoo::tls::validate_tls_material(cert_pem, key_pem)"),
        "worker TLS path must delegate to shared TLS validation kernel"
    );
}

#[test]
fn worker_reload_uses_single_helper_path() {
    let worker_source = include_str!("../src/bin/pishoo_worker.rs");

    assert!(
        worker_source.matches("build_worker_reload_plan(").count() >= 3,
        "worker should route startup + reload through the shared plan builder"
    );
    assert!(
        worker_source.matches("apply_worker_reload_plan(").count() >= 3,
        "worker should route startup + reload through the shared apply helper"
    );
    assert_eq!(
        worker_source.matches("request_listen(").count(),
        1,
        "request_listen should only be issued inside the reload apply helper"
    );
    assert_eq!(
        worker_source.matches("release_listen(").count(),
        1,
        "release_listen should only be issued inside the reload apply helper"
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
        worker_source.contains("stop_server_runtimes(&listeners).await;"),
        "worker shutdown must explicitly stop listener runtimes before exit"
    );
}

#[test]
fn worker_reload_rebuilds_all_listener_handles() {
    let worker_source = include_str!("../src/bin/pishoo_worker.rs");
    assert!(
        !worker_source.contains("same_listener_request(")
            && worker_source.contains("for (server_name, runtime) in current_runtimes"),
        "worker reload should release all current listeners before rebuilding them"
    );
}
