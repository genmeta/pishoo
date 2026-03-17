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
        worker_source.contains("tls::validate_tls_material(&cert_pem, &key_pem)"),
        "worker TLS path must delegate to shared TLS validation kernel"
    );
}

#[test]
fn worker_reconcile_uses_single_helper_path() {
    let worker_source = include_str!("../src/bin/pishoo_worker.rs");

    assert_eq!(
        worker_source.matches("reconcile_listener_set(").count(),
        3,
        "worker should have one helper definition and two call sites (startup + reload)"
    );
    assert_eq!(
        worker_source.matches("root_api.request_listen(").count(),
        1,
        "request_listen should only be issued inside the reconcile helper"
    );
    assert_eq!(
        worker_source.matches("root_api.release_listen(").count(),
        1,
        "release_listen should only be issued inside the reconcile helper"
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
