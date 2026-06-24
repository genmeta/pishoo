#[test]
fn worker_requests_root_owned_connector() {
    let worker_source = include_str!("../src/bin/pishoo_worker.rs");

    assert!(
        worker_source.contains("RemoteControlPlane::new(")
            && worker_source.contains("bootstrap.control_plane"),
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
fn local_connector_type_is_dhttp_endpoint() {
    use gateway::control_plane::ProvideConnector;

    type Connector =
        <pishoo::hypervisor::in_process_plane::InProcessControlPlane as ProvideConnector>::Connector;

    fn assert_same_type(
        value: Option<Connector>,
    ) -> Option<std::sync::Arc<dhttp::endpoint::Endpoint>> {
        value
    }

    let _ = assert_same_type;
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
fn listener_rebuild_uses_control_plane_rebuild_call() {
    let ipc_source = include_str!("../src/ipc.rs");
    let remote_source = include_str!("../src/worker/remote_plane.rs");
    let local_source = include_str!("../src/hypervisor/in_process_plane.rs");
    let ipc_server_source = include_str!("../src/hypervisor/ipc_server.rs");

    assert!(
        ipc_source.contains("async fn rebuild_listener"),
        "ipc ControlPlane trait must expose async fn rebuild_listener"
    );
    assert!(
        ipc_source.contains("RebuildListenError"),
        "ipc must define a dedicated RebuildListenError type"
    );
    assert!(
        ipc_server_source.contains("rebuild_listener"),
        "root WorkerControlPlane must implement rebuild_listener"
    );
    assert!(
        remote_source.contains("rebuild_listener"),
        "RemoteControlPlane must expose rebuild_listener consuming the old IpcListener"
    );
    assert!(
        local_source.contains("rebuild_listener"),
        "InProcessControlPlane must expose rebuild_listener consuming the old RegisteredEndpoint"
    );
}

#[test]
fn sshd_service_registers_webtransport_protocol_layer() {
    let service_source = include_str!("../src/service/snapshot.rs");

    assert!(
        service_source.contains("WebTransportProtocolFactory"),
        "sshd services must register h3x WebTransport protocol routing"
    );
    assert!(
        !service_source.contains("Ssh3ProtocolFactory"),
        "sshd services must not keep the legacy DShell stream protocol routing"
    );
}

#[test]
fn accept_state_aborts_listener_task_on_drop() {
    let service_source = include_str!("../src/service.rs");
    let accept_source = include_str!("../src/service/accept.rs");

    assert!(
        service_source.contains("pub mod accept;"),
        "service module must expose the accept submodule"
    );
    assert!(
        accept_source.contains("pub enum AcceptState"),
        "service::accept must define an AcceptState enum"
    );
    assert!(
        accept_source.contains("task: AbortOnDropHandle<L>"),
        "AcceptState::Running must abort its accept task if dropped before explicit shutdown"
    );
    assert!(
        !accept_source.contains("listener: L,\n        service:")
            && !accept_source.contains("listener: L,\n        task:"),
        "AcceptState must not hold listener and task side by side; the listener \
         must be owned by the spawned task"
    );
}

#[test]
fn worker_reload_uses_worker_runtime() {
    let worker_source = include_str!("../src/bin/pishoo_worker.rs");

    assert!(
        worker_source.contains("WorkerRuntime::new(")
            && worker_source.contains("runtime.reload().await")
            && worker_source.contains("runtime.shutdown().await"),
        "worker must delegate reload/shutdown to WorkerRuntime"
    );
    assert!(!worker_source.contains("setup_service("));
    assert!(!worker_source.contains("service::run_service("));
    assert!(!worker_source.contains("collect_reusable_listeners("));
}

#[test]
fn worker_uses_dhttp_home_api_for_user_home() {
    let worker_source = include_str!("../src/bin/pishoo_worker.rs");

    assert!(
        worker_source.contains("DhttpHome::for_user_home_dir"),
        "worker must construct user dhttp home through the dhttp home API"
    );
    assert!(
        !worker_source.contains("join(\".dhttp\")"),
        "worker must not hard-code the dhttp home directory layout"
    );
}
