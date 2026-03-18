use std::sync::{Arc, Once, OnceLock};

use nix::unistd::{Pid, Uid};
use pishoo::{
    protocol::{OpenConnector, OpenConnectorError},
    root_state::RootState,
    worker_spawn::WorkerHandle,
};

fn quic_test_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn install_crypto_provider() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

struct SharedQuicFixture {
    listeners: Arc<gm_quic::prelude::QuicListeners>,
    client: Arc<gm_quic::prelude::QuicClient>,
}

fn shared_quic_fixture() -> &'static SharedQuicFixture {
    use gm_quic::prelude::handy::{ToCertificate, server_parameters};

    static FIXTURE: OnceLock<SharedQuicFixture> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        install_crypto_provider();

        let mut root_store = rustls::RootCertStore::empty();
        let root_cert = include_bytes!("../../keychain/root.crt");
        root_store.add_parsable_certificates(root_cert.to_certificate());
        let roots = Arc::new(root_store);

        let listeners = gm_quic::prelude::QuicListeners::builder()
            .with_parameters(server_parameters())
            .with_client_cert_verifier(
                rustls::server::WebPkiClientVerifier::builder(roots)
                    .allow_unauthenticated()
                    .build()
                    .expect("build verifier"),
            )
            .with_alpns([b"h3".as_slice()])
            .listen(16)
            .expect("create listeners");

        let client = Arc::new(
            gm_quic::prelude::QuicClient::builder()
                .with_root_certificates(Arc::new(rustls::RootCertStore::empty()))
                .without_cert()
                .with_alpns(vec!["h3"])
                .build(),
        );

        SharedQuicFixture { listeners, client }
    })
}

fn test_root_state() -> RootState {
    let fixture = shared_quic_fixture();
    RootState::new(fixture.listeners.clone(), fixture.client.clone())
}

fn spawn_test_worker() -> WorkerHandle {
    WorkerHandle::new(
        tokio::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("spawn worker child"),
    )
}

#[tokio::test(flavor = "current_thread")]
async fn registered_worker_open_connector_tracks_shutdown_token() {
    let _guard = quic_test_lock().lock().await;
    let mut state = test_root_state();
    let pid = Pid::from_raw(1101);
    let uid = Uid::from_raw(2101);

    state.register_worker(pid, uid, spawn_test_worker());

    let connector = state
        .open_connector(
            pid,
            OpenConnector {
                profile: String::new(),
            },
        )
        .await
        .expect("registered worker should open connector");

    let process = state.get_process(pid).expect("process must remain registered");
    assert_eq!(process.connector_shutdown_tokens.len(), 1);

    drop(connector);
    let _ = state.cleanup_worker_with_reason(pid, "test_end");
}

#[tokio::test(flavor = "current_thread")]
async fn unknown_worker_pid_cannot_open_connector() {
    let _guard = quic_test_lock().lock().await;
    let mut state = test_root_state();
    let err = state
        .open_connector(
            Pid::from_raw(9191),
            OpenConnector {
                profile: String::new(),
            },
        )
        .await
        .expect_err("unknown worker pid must be rejected");

    assert!(matches!(
        err,
        OpenConnectorError::Internal { ref message } if message.contains("unknown caller pid")
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn cleanup_cancels_connector_tokens() {
    let _guard = quic_test_lock().lock().await;
    let mut state = test_root_state();
    let pid = Pid::from_raw(1102);
    let uid = Uid::from_raw(2102);

    state.register_worker(pid, uid, spawn_test_worker());
    state
        .open_connector(
            pid,
            OpenConnector {
                profile: String::new(),
            },
        )
        .await
        .expect("registered worker should open connector");

    let token = state
        .get_process(pid)
        .and_then(|process| process.connector_shutdown_tokens.first().cloned())
        .expect("connector shutdown token should be tracked");

    let summary = state
        .cleanup_worker_with_reason(pid, "integration_test")
        .expect("cleanup summary should exist");

    assert_eq!(summary.connectors_cleaned, 1);
    assert!(token.is_cancelled());
}

#[test]
fn root_state_uses_shared_tls_validation() {
    let root_state_source = include_str!("../src/root_state.rs");
    assert!(
        root_state_source.contains("tls::validate_tls_material(cert_pem, key_pem)"),
        "root_state must delegate to shared TLS validation kernel"
    );
}

#[test]
fn release_and_cleanup_share_retire_server_helper() {
    let root_state_source = include_str!("../src/root_state.rs");

    assert_eq!(
        root_state_source.matches("retire_server(").count(),
        3,
        "root_state should have one retire helper definition and two call sites"
    );
    assert!(
        root_state_source.contains("if self.retire_server(server_name).is_some()"),
        "cleanup path must use retire_server helper"
    );
    assert!(
        root_state_source.contains(
            "self.retire_server(server_name).expect(\"server must exist after ownership check\")"
        ),
        "release_listen path must use retire_server helper"
    );
}

#[test]
fn release_listen_updates_owned_servers() {
    let root_state_source = include_str!("../src/root_state.rs");

    assert!(
        root_state_source.contains("process.owned_servers.remove(server_name);"),
        "release_listen must remove the server from the worker ownership set"
    );
    assert!(
        root_state_source.contains("connectors_cleaned = summary.connectors_cleaned"),
        "cleanup logging must still report connector cleanup counts"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn cleanup_regression_removes_process_and_uid_mapping() {
    let _guard = quic_test_lock().lock().await;
    let mut state = test_root_state();
    let pid = Pid::from_raw(1201);
    let uid = Uid::from_raw(2201);

    state.register_worker(pid, uid, spawn_test_worker());
    state
        .open_connector(
            pid,
            OpenConnector {
                profile: String::new(),
            },
        )
        .await
        .expect("registered worker should open connector before cleanup");

    let summary = state
        .cleanup_worker_with_reason(pid, "cleanup_regression")
        .expect("cleanup summary should exist");

    assert_eq!(summary.pid, pid);
    assert_eq!(summary.uid, uid);
    assert_eq!(summary.servers_cleaned, 0);
    assert_eq!(summary.connectors_cleaned, 1);
    assert!(state.get_process(pid).is_none(), "cleanup must remove process record");
    assert!(
        state.get_pid_for_uid(uid).is_none(),
        "cleanup must remove uid mapping for cleaned pid"
    );
}
