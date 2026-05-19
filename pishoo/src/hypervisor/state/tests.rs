use std::sync::Arc;

use dhttp::{ddns::PublishOptions, identity::Identity, name::DhttpName};
use gateway::control_plane::ListenRequest;
use nix::unistd::{Pid, Uid};

use super::*;
use crate::hypervisor::worker_handle::WorkerHandle;

/// Test helper: is there an `Active` entry for `server_name`?
async fn is_active(state: &RootState, server_name: &str) -> bool {
    let server_name = DhttpName::try_from_str_full(server_name.to_owned()).unwrap();
    let registry = state.servers.read().await;
    matches!(
        registry.entries.get(&server_name),
        Some(ServerEntry::Active { .. })
    )
}

fn dhttp_name(label: &str) -> DhttpName<'static> {
    DhttpName::try_from_str_full(format!("{label}.user.genmeta.net")).unwrap()
}

// -----------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------

/// Create an independent [`Network`] + [`RootState`] for one test.
fn test_state() -> Arc<RootState> {
    // Ensure the rustls CryptoProvider is installed (idempotent).
    let _ = rustls::crypto::ring::default_provider().install_default();

    let network = h3x::dquic::Network::builder()
        .stun_server(Arc::<str>::from(""))
        .build();
    Arc::new(RootState::new(
        network,
        h3x::dquic::server::ServerQuicConfig::default(),
    ))
}

/// Generate a self-signed Identity for `{label}.user.genmeta.net`.
fn test_identity(label: &str) -> Identity {
    let fqdn = format!("{label}.user.genmeta.net");
    let key_pair = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::new(vec![fqdn.clone()]).unwrap();
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, &fqdn);
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
        publish_options: PublishOptions::default(),
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
        .cleanup_worker_with_reason(Pid::from_raw(77777), "test")
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

    let first = state.cleanup_worker_with_reason(pid, "exit").await;
    assert!(first.is_some());

    let second = state.cleanup_worker_with_reason(pid, "exit").await;
    assert!(second.is_none());
}

#[tokio::test]
async fn test_spawn_task_unregistered() {
    let state = test_state();
    let (tx, mut rx) = tokio::sync::oneshot::channel();

    // Task for an unregistered PID should not be spawned.
    state
        .spawn_worker_task(Pid::from_raw(55555), async move {
            let _ = tx.send(());
        })
        .await;

    // Give the runtime a chance to run the task (if it were spawned).
    tokio::task::yield_now().await;
    assert!(rx.try_recv().is_err(), "task should not have been spawned");
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

    state.cleanup_worker_with_reason(p1, "exit").await;

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
    let owner = ServiceOwner::Local;
    let server_name = "reg-release.user.genmeta.net";

    let _listener = state
        .register_listener(owner, test_request("reg-release"))
        .await
        .expect("register_listener should succeed");

    assert!(is_active(&state, server_name).await);

    state
        .release_server(
            &DhttpName::try_from_str_full(server_name.to_owned()).unwrap(),
            owner,
        )
        .await;
    assert!(!is_active(&state, server_name).await);
}

#[tokio::test]
async fn test_register_duplicate_same_owner() {
    let state = test_state();
    let owner = ServiceOwner::Local;

    let _listener = state
        .register_listener(owner, test_request("dup-same"))
        .await
        .expect("first register should succeed");

    let err = state
        .register_listener(owner, test_request("dup-same"))
        .await
        .err()
        .expect("second register should fail");

    assert!(matches!(err, RegisterError::DuplicateListen));
}

#[tokio::test]
async fn test_register_cross_owner_conflict() {
    let state = test_state();
    let owner_local = ServiceOwner::Local;
    let pid = Pid::from_raw(33333);
    let uid = Uid::from_raw(3000);
    let owner_worker = ServiceOwner::Worker(pid);

    state
        .register_worker(pid, uid, "worker".into(), fake_worker_handle(33333))
        .await;

    let _listener = state
        .register_listener(owner_local, test_request("cross-conflict"))
        .await
        .expect("first register should succeed");

    // Different owner for same name → ConflictedName.
    let err = state
        .register_listener(owner_worker, test_request("cross-conflict"))
        .await
        .err()
        .expect("cross-owner should conflict");

    assert!(matches!(err, RegisterError::ConflictedName));

    // Original server's conn_sender should be None (poisoned).
    assert!(!is_active(&state, "cross-conflict.user.genmeta.net").await);
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
        .register_listener(ServiceOwner::Local, test_request("on-conflict"))
        .await
        .unwrap();
    let _ = state
        .register_listener(ServiceOwner::Worker(pid), test_request("on-conflict"))
        .await;

    // Name is now Conflicted. Any new register should fail.
    let err = state
        .register_listener(ServiceOwner::Local, test_request("on-conflict"))
        .await
        .err()
        .expect("register on conflicted should fail");
    assert!(matches!(err, RegisterError::ConflictedName));
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
        .register_listener(ServiceOwner::Local, test_request("scrub-re"))
        .await
        .unwrap();
    let _ = state
        .register_listener(ServiceOwner::Worker(pid), test_request("scrub-re"))
        .await;

    // Scrub should clear the conflict.
    let scrubbed = state.scrub_conflicts().await;
    assert!(scrubbed.contains(&dhttp_name("scrub-re")));

    // Re-register should now succeed.
    let _listener = state
        .register_listener(ServiceOwner::Local, test_request("scrub-re"))
        .await
        .expect("re-register after scrub should succeed");
    assert!(is_active(&state, "scrub-re.user.genmeta.net").await);
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
        .register_listener(ServiceOwner::Local, test_request("wrong-owner"))
        .await
        .unwrap();

    // Release with wrong owner should be a no-op.
    state
        .release_server(&dhttp_name("wrong-owner"), ServiceOwner::Worker(pid))
        .await;

    // Server should still be active.
    assert!(is_active(&state, "wrong-owner.user.genmeta.net").await);
}

#[tokio::test]
async fn test_release_nonexistent() {
    let state = test_state();
    // Should not panic.
    state
        .release_server(&dhttp_name("does-not-exist"), ServiceOwner::Local)
        .await;
}

#[tokio::test]
async fn test_cleanup_worker_releases_servers() {
    let state = test_state();
    let pid = Pid::from_raw(10001);
    let uid = Uid::from_raw(6000);
    let owner = ServiceOwner::Worker(pid);

    state
        .register_worker(pid, uid, "cleaner".into(), fake_worker_handle(10001))
        .await;

    let _listener = state
        .register_listener(owner, test_request("cleanup-srv"))
        .await
        .expect("register should succeed");
    assert!(is_active(&state, "cleanup-srv.user.genmeta.net").await);

    // Cleanup the worker — its server should also be gone.
    let summary = state
        .cleanup_worker_with_reason(pid, "exit")
        .await
        .expect("cleanup should find the worker");
    assert_eq!(summary.servers_cleaned, 1);

    assert!(!is_active(&state, "cleanup-srv.user.genmeta.net").await);
    assert!(!state.has_worker(pid).await);
}
