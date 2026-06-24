use std::{
    ffi::CString,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use dhttp::{
    certificate::{
        CertificateChainKey, CertificateChainKind, CertificateSequence, DhttpSubjectKeyIdentifier,
        OwnerHash,
    },
    identity::Identity,
    name::DhttpName,
};
use gateway::{
    control_plane::ListenRequest,
    parse::types::{IfaceRange, IpFamilies, Listens},
};
use nix::unistd::{Pid, Uid};

use super::{owner::Owner, *};
use crate::hypervisor::worker_handle::WorkerHandle;

/// Test helper: is there an `Active` entry for `server_name`?
async fn is_active(state: &RootState, server_name: &str) -> bool {
    let server_name = DhttpName::try_from(server_name.to_owned()).unwrap();
    let registry = state.listeners.read().await;
    registry.is_active(&server_name)
}

async fn has_entry(state: &RootState, server_name: &str) -> bool {
    let server_name = DhttpName::try_from(server_name.to_owned()).unwrap();
    let registry = state.listeners.read().await;
    registry.contains(&server_name)
}

async fn wait_until_no_entry(state: &RootState, server_name: &str) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
    loop {
        if !has_entry(state, server_name).await {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "listener registry entry `{server_name}` should be removed"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

fn dhttp_name(label: &str) -> DhttpName<'static> {
    DhttpName::try_from(format!("{label}.user.dhttp.net")).unwrap()
}

fn dhttp_subject_key_identifier_der() -> Vec<u8> {
    let owner_hash =
        OwnerHash::try_from("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
            .unwrap();
    let value = DhttpSubjectKeyIdentifier::new(
        CertificateChainKey::new(
            CertificateSequence::from(0u8),
            CertificateChainKind::Primary,
        ),
        owner_hash,
    )
    .to_string();

    let bytes = value.as_bytes();
    assert!(bytes.len() < 128, "test dhttp ski must use short-form DER");
    let mut der = Vec::with_capacity(bytes.len() + 2);
    der.push(0x04);
    der.push(bytes.len() as u8);
    der.extend_from_slice(bytes);
    der
}

// -----------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------

/// Create an independent [`Network`] + [`RootState`] for one test.
fn test_state() -> Arc<RootState> {
    // Ensure the rustls CryptoProvider is installed (idempotent).
    let _ = rustls::crypto::ring::default_provider().install_default();

    let network = dhttp::h3x::dquic::Network::builder()
        .stun_server(Arc::<str>::from(""))
        .build();
    Arc::new(RootState::new(dhttp::network::DhttpNetwork::from(network)))
}

/// Generate a self-signed Identity for `{label}.user.dhttp.net`.
fn test_identity(label: &str) -> Identity {
    let fqdn = format!("{label}.user.dhttp.net");
    let key_pair = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::new(vec![fqdn.clone()]).unwrap();
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, &fqdn);
    params
        .custom_extensions
        .push(rcgen::CustomExtension::from_oid_content(
            &[2, 5, 29, 14],
            dhttp_subject_key_identifier_der(),
        ));
    let cert = params.self_signed(&key_pair).unwrap();
    let cert_der = rustls::pki_types::CertificateDer::from(cert.der().to_vec());
    let key_der = rustls::pki_types::PrivateKeyDer::try_from(key_pair.serialize_der()).unwrap();
    Identity::new(dhttp_name(label).into(), vec![cert_der], key_der)
}

/// Build a ListenRequest with no bind addresses (no actual sockets).
fn test_request(label: &str) -> ListenRequest {
    ListenRequest {
        identity: test_identity(label),
        bind: vec![],
        dns_resolver_url: None,
    }
}

fn test_request_with_bind(label: &str, bind: Vec<Listens>) -> ListenRequest {
    ListenRequest {
        bind,
        ..test_request(label)
    }
}

/// Create a WorkerHandle from a raw PID value.
///
/// Uses PID 1 (init) by default in tests — never killed because our
/// tests never call force_kill / drop the handle with intent to reap.
fn fake_worker_handle(raw: i32) -> WorkerHandle {
    WorkerHandle::from_pid(Pid::from_raw(raw))
}

// -----------------------------------------------------------------------
// Phase A: Worker registry
// -----------------------------------------------------------------------

#[tokio::test]
async fn test_register_worker_basic() {
    let state = test_state();
    let pid = Pid::from_raw(99999);
    let uid = Uid::from_raw(1000);

    state
        .register_worker(pid, uid, "alice".into(), fake_worker_handle(99999))
        .await;

    assert!(state.has_worker(pid).await);
    assert_eq!(state.pid_for_uid(uid).await, Some(pid));
    assert!(state.worker_pids().await.contains(&pid));
}

#[tokio::test]
async fn test_register_worker_uid_replacement() {
    let state = test_state();
    let uid = Uid::from_raw(1001);
    let old_pid = Pid::from_raw(88881);
    let new_pid = Pid::from_raw(88882);

    state
        .register_worker(old_pid, uid, "bob".into(), fake_worker_handle(88881))
        .await;
    assert!(state.has_worker(old_pid).await);

    // Same UID, different PID → old worker is cleaned up.
    state
        .register_worker(new_pid, uid, "bob".into(), fake_worker_handle(88882))
        .await;

    assert!(!state.has_worker(old_pid).await);
    assert!(state.has_worker(new_pid).await);
    assert_eq!(state.pid_for_uid(uid).await, Some(new_pid));
}

#[tokio::test]
async fn test_cleanup_unknown_pid() {
    let state = test_state();
    let result = state
        .cleanup_worker(Pid::from_raw(77777), WorkerProcessError::RootShutdown)
        .await;
    assert!(result.is_none());
}

#[tokio::test]
async fn test_cleanup_idempotent() {
    let state = test_state();
    let pid = Pid::from_raw(66666);
    state
        .register_worker(
            pid,
            Uid::from_raw(1002),
            "carol".into(),
            fake_worker_handle(66666),
        )
        .await;

    let first = state
        .cleanup_worker(
            pid,
            WorkerProcessError::ChildExited {
                status: nix::sys::wait::WaitStatus::Exited(pid, 1),
            },
        )
        .await;
    assert!(first.is_some());

    let second = state
        .cleanup_worker(pid, WorkerProcessError::RootShutdown)
        .await;
    assert!(second.is_none());
}

#[tokio::test]
async fn test_spawn_task_unregistered() {
    let state = test_state();
    let (tx, mut rx) = tokio::sync::oneshot::channel();

    // Task for an unregistered PID should not be spawned.
    let spawned = state
        .spawn_worker_task(Pid::from_raw(55555), |_token| async move {
            let _ = tx.send(());
        })
        .await;

    // Give the runtime a chance to run the task (if it were spawned).
    tokio::task::yield_now().await;
    assert!(!spawned, "unregistered worker task should be rejected");
    assert!(rx.try_recv().is_err(), "task should not have been spawned");
}

#[tokio::test]
async fn test_cleanup_cancels_and_waits_worker_tasks() {
    let state = test_state();
    let pid = Pid::from_raw(55556);
    state
        .register_worker(
            pid,
            Uid::from_raw(1003),
            "cancel-wait".into(),
            fake_worker_handle(55556),
        )
        .await;

    let observed_cancel = Arc::new(AtomicBool::new(false));
    let completed_cleanup = Arc::new(AtomicBool::new(false));
    let observed_cancel_for_task = observed_cancel.clone();
    let completed_cleanup_for_task = completed_cleanup.clone();

    let spawned = state
        .spawn_worker_task(pid, move |token| async move {
            token.cancelled().await;
            observed_cancel_for_task.store(true, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            completed_cleanup_for_task.store(true, Ordering::SeqCst);
        })
        .await;
    assert!(spawned, "registered worker task should be tracked");

    state
        .cleanup_worker(pid, WorkerProcessError::RootShutdown)
        .await
        .expect("cleanup should find worker");

    assert!(
        observed_cancel.load(Ordering::SeqCst),
        "worker task should observe cancellation"
    );
    assert!(
        completed_cleanup.load(Ordering::SeqCst),
        "cleanup should wait for worker task to finish"
    );
}

#[tokio::test]
async fn test_worker_pids_after_cleanup() {
    let state = test_state();
    let p1 = Pid::from_raw(44441);
    let p2 = Pid::from_raw(44442);

    state
        .register_worker(
            p1,
            Uid::from_raw(2001),
            "d".into(),
            fake_worker_handle(44441),
        )
        .await;
    state
        .register_worker(
            p2,
            Uid::from_raw(2002),
            "e".into(),
            fake_worker_handle(44442),
        )
        .await;

    assert_eq!(state.worker_pids().await.len(), 2);

    state
        .cleanup_worker(p1, WorkerProcessError::RootShutdown)
        .await;

    let pids = state.worker_pids().await;
    assert_eq!(pids.len(), 1);
    assert!(pids.contains(&p2));
}

// -----------------------------------------------------------------------
// Phase B: Server lifecycle (via public API)
// -----------------------------------------------------------------------

#[tokio::test]
async fn test_register_then_release() {
    let state = test_state();
    let owner = Owner::Local;
    let server_name = "reg-release.user.dhttp.net";

    let _listener = state
        .acquire_listener(owner, test_request("reg-release"))
        .await
        .expect("acquire_listener should succeed");

    assert!(is_active(&state, server_name).await);

    state
        .release_listener(owner, &DhttpName::try_from(server_name.to_owned()).unwrap())
        .await
        .expect("release should succeed");
    assert!(!is_active(&state, server_name).await);
}

#[tokio::test]
async fn test_registered_endpoint_drop_releases_listener() {
    let state = test_state();
    let owner = Owner::Local;
    let server_name = "drop-release.user.dhttp.net";

    let listener = state
        .acquire_listener(owner, test_request("drop-release"))
        .await
        .expect("acquire_listener should succeed");
    assert!(is_active(&state, server_name).await);

    drop(listener);

    wait_until_no_entry(&state, server_name).await;
}

#[tokio::test]
async fn test_cancelled_release_listener_still_finishes_destroy() {
    let state = test_state();
    let owner = Owner::Local;
    let server_name = "cancel-release.user.dhttp.net";
    let server_name_owned = DhttpName::try_from(server_name.to_owned()).unwrap();

    let _listener = state
        .acquire_listener(owner, test_request("cancel-release"))
        .await
        .expect("acquire_listener should succeed");
    assert!(is_active(&state, server_name).await);

    let destroy_pause = state.pause_next_listener_destroy_for_test();
    let release_state = state.clone();
    let release_name = server_name_owned.clone();
    let release = tokio::spawn(async move {
        release_state
            .release_listener(owner, &release_name)
            .await
            .expect("release should succeed");
    });

    destroy_pause.wait_started().await;
    release.abort();
    destroy_pause.resume();

    wait_until_no_entry(&state, server_name).await;
}

#[tokio::test]
async fn test_cancelled_acquire_listener_destroys_built_endpoint() {
    let state = test_state();
    let owner = Owner::Local;
    let server_name = "cancel-acquire.user.dhttp.net";

    let delivery_pause = state.pause_next_listener_delivery_for_test();
    let acquire_state = state.clone();
    let acquire = tokio::spawn(async move {
        acquire_state
            .acquire_listener(owner, test_request("cancel-acquire"))
            .await
            .expect("acquire should succeed");
    });

    delivery_pause.wait_started().await;
    acquire.abort();
    delivery_pause.resume();

    wait_until_no_entry(&state, server_name).await;
}

#[tokio::test]
async fn test_stale_registered_endpoint_drop_after_rebuild_does_not_release_replacement() {
    let state = test_state();
    let owner = Owner::Local;
    let server_name = "stale-rebuild.user.dhttp.net";

    let old_listener = state
        .acquire_listener(owner, test_request("stale-rebuild"))
        .await
        .expect("initial acquire should succeed");
    assert!(is_active(&state, server_name).await);

    let _replacement = state
        .rebuild_listener(owner, test_request("stale-rebuild"))
        .await
        .expect("rebuild should succeed");

    drop(old_listener);

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        is_active(&state, server_name).await,
        "dropping the consumed old handle must not release the replacement"
    );

    state
        .release_listener(owner, &DhttpName::try_from(server_name.to_owned()).unwrap())
        .await
        .expect("release should succeed");
}

#[tokio::test]
async fn test_cleanup_local_resources_retires_config_services() {
    let state = test_state();
    let server_name = "cleanup-local.user.dhttp.net";
    let listener = state
        .acquire_listener(Owner::Local, test_request("cleanup-local"))
        .await
        .expect("acquire_listener should succeed");

    assert!(is_active(&state, server_name).await);

    let cleaned = state.cleanup_local_resources().await;

    assert_eq!(cleaned, 1);
    assert!(!is_active(&state, server_name).await);

    drop(listener);
}

#[tokio::test]
async fn test_acquire_listener_rejects_external_listen_without_registry_entry() {
    let state = test_state();
    let owner = Owner::Local;

    let err = state
        .acquire_listener(
            owner,
            test_request_with_bind(
                "external-listen",
                vec![Listens::new(IfaceRange::External, IpFamilies::Dual, 443)],
            ),
        )
        .await
        .err()
        .expect("external listen should fail");

    assert!(matches!(
        err,
        AcquireListenerError::BuildBindPatterns { .. }
    ));
    assert!(!has_entry(&state, "external-listen.user.dhttp.net").await);
}

#[tokio::test]
async fn test_register_duplicate_same_owner() {
    let state = test_state();
    let owner = Owner::Local;

    let _listener = state
        .acquire_listener(owner, test_request("dup-same"))
        .await
        .expect("first register should succeed");

    let err = state
        .acquire_listener(owner, test_request("dup-same"))
        .await
        .err()
        .expect("second register should fail");

    assert!(matches!(err, AcquireListenerError::DuplicateListen));
}

#[tokio::test]
async fn test_register_cross_owner_conflict() {
    let state = test_state();
    let owner_local = Owner::Local;
    let pid = Pid::from_raw(33333);
    let uid = Uid::from_raw(3000);
    let owner_worker = Owner::worker(uid, pid);

    state
        .register_worker(pid, uid, "worker".into(), fake_worker_handle(33333))
        .await;

    let _listener = state
        .acquire_listener(owner_local, test_request("cross-conflict"))
        .await
        .expect("first register should succeed");

    // Different owner for same name → ConflictedName.
    let err = state
        .acquire_listener(owner_worker, test_request("cross-conflict"))
        .await
        .err()
        .expect("cross-owner should conflict");

    assert!(matches!(err, AcquireListenerError::ConflictedName));

    // Original server's conn_sender should be None (poisoned).
    assert!(!is_active(&state, "cross-conflict.user.dhttp.net").await);
}

#[tokio::test]
async fn test_register_on_conflicted() {
    let state = test_state();
    let pid = Pid::from_raw(22221);
    state
        .register_worker(
            pid,
            Uid::from_raw(4000),
            "w1".into(),
            fake_worker_handle(22221),
        )
        .await;

    // Create a conflict first.
    let _l = state
        .acquire_listener(Owner::Local, test_request("on-conflict"))
        .await
        .unwrap();
    let _ = state
        .acquire_listener(
            Owner::worker(Uid::from_raw(4000), pid),
            test_request("on-conflict"),
        )
        .await;

    // Name is now Conflicted. Any new register should fail.
    let err = state
        .acquire_listener(Owner::Local, test_request("on-conflict"))
        .await
        .err()
        .expect("register on conflicted should fail");
    assert!(matches!(err, AcquireListenerError::ConflictedName));
}

#[tokio::test]
async fn test_scrub_then_reregister() {
    let state = test_state();
    let pid = Pid::from_raw(22222);
    state
        .register_worker(
            pid,
            Uid::from_raw(4001),
            "w2".into(),
            fake_worker_handle(22222),
        )
        .await;

    // Create conflict.
    let _l = state
        .acquire_listener(Owner::Local, test_request("scrub-re"))
        .await
        .unwrap();
    let _ = state
        .acquire_listener(
            Owner::worker(Uid::from_raw(4001), pid),
            test_request("scrub-re"),
        )
        .await;

    // Scrub should clear the conflict.
    let scrubbed = state.clear_listener_poison().await;
    assert!(scrubbed.contains(&dhttp_name("scrub-re")));

    // Re-register should now succeed.
    let _listener = state
        .acquire_listener(Owner::Local, test_request("scrub-re"))
        .await
        .expect("re-register after scrub should succeed");
    assert!(is_active(&state, "scrub-re.user.dhttp.net").await);
}

#[tokio::test]
async fn test_release_wrong_owner() {
    let state = test_state();
    let pid = Pid::from_raw(11111);
    state
        .register_worker(
            pid,
            Uid::from_raw(5000),
            "w3".into(),
            fake_worker_handle(11111),
        )
        .await;

    let _listener = state
        .acquire_listener(Owner::Local, test_request("wrong-owner"))
        .await
        .unwrap();

    // Release with wrong owner should return a typed error.
    state
        .release_listener(
            Owner::worker(Uid::from_raw(5000), pid),
            &dhttp_name("wrong-owner"),
        )
        .await
        .expect_err("wrong-owner release should be typed");

    // Server should still be active.
    assert!(is_active(&state, "wrong-owner.user.dhttp.net").await);
}

#[tokio::test]
async fn test_release_nonexistent() {
    let state = test_state();
    // Should not panic.
    state
        .release_listener(Owner::Local, &dhttp_name("does-not-exist"))
        .await
        .expect("release of nonexistent listener should succeed");
}

#[tokio::test]
async fn test_cleanup_worker_releases_servers() {
    let state = test_state();
    let pid = Pid::from_raw(10001);
    let uid = Uid::from_raw(6000);
    let owner = Owner::worker(uid, pid);

    state
        .register_worker(pid, uid, "cleaner".into(), fake_worker_handle(10001))
        .await;

    let _listener = state
        .acquire_listener(owner, test_request("cleanup-srv"))
        .await
        .expect("register should succeed");
    assert!(is_active(&state, "cleanup-srv.user.dhttp.net").await);

    // Cleanup the worker — its server should also be gone.
    let summary = state
        .cleanup_worker(
            pid,
            WorkerProcessError::ChildExited {
                status: nix::sys::wait::WaitStatus::Exited(pid, 1),
            },
        )
        .await
        .expect("cleanup should find the worker");
    assert_eq!(summary.servers_cleaned, 1);

    assert!(!is_active(&state, "cleanup-srv.user.dhttp.net").await);
    assert!(!state.has_worker(pid).await);
}

#[tokio::test]
async fn test_fail_worker_queues_typed_error_without_cleanup() {
    let state = test_state();
    let pid = Pid::from_raw(10002);
    state
        .register_worker(
            pid,
            Uid::from_raw(6001),
            "failed-worker".into(),
            fake_worker_handle(10002),
        )
        .await;

    state
        .fail_worker(pid, WorkerProcessError::IpcDisconnected)
        .await;

    assert!(
        state.has_worker(pid).await,
        "recording failure must not clean up from inside a worker task"
    );

    let failures = state.collect_worker_failures().await;
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].pid, pid);
    assert!(matches!(
        &failures[0].error,
        WorkerProcessError::IpcDisconnected
    ));
}

#[tokio::test]
async fn test_desired_worker_targets_are_uid_keyed() {
    let state = test_state();
    let target = nix::unistd::User {
        name: "restart-user".to_owned(),
        passwd: CString::new("x").unwrap(),
        uid: Uid::from_raw(7001),
        gid: nix::unistd::Gid::from_raw(7001),
        gecos: CString::new("").unwrap(),
        dir: std::path::PathBuf::from("/home/restart-user"),
        shell: std::path::PathBuf::from("/bin/sh"),
    };

    state.set_desired_workers(vec![target.clone()]).await;

    assert_eq!(
        state
            .desired_worker_target(Uid::from_raw(7001))
            .await
            .map(|worker| worker.name),
        Some(target.name)
    );
    assert!(
        state
            .desired_worker_target(Uid::from_raw(7002))
            .await
            .is_none()
    );
}
