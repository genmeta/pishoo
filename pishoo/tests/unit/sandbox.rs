use super::*;

#[test]
fn identities_have_independent_capacity_and_shared_handles_keep_one_budget() {
    let alice = Arc::new(Sandbox::new());
    let alice_router = alice.clone();
    let bob = Sandbox::new();
    let permits = (0..4)
        .map(|_| alice_router.lib_slots.clone().try_acquire_owned().unwrap())
        .collect::<Vec<_>>();
    assert!(alice.lib_slots.clone().try_acquire_owned().is_err());
    assert_eq!(bob.lib_slots.available_permits(), 4);

    alice.close();
    alice.close();
    assert!(alice_router.lib_slots.is_closed());
    assert!(alice_router.tasks.is_closed());
    assert!(!bob.lib_slots.is_closed());
    assert!(!bob.tasks.is_closed());
    assert!(bob.lib_slots.clone().try_acquire_owned().is_ok());
    // Closing admission does not return permits held by active executions.
    assert_eq!(alice.lib_slots.available_permits(), 0);
    drop(permits);
    assert_eq!(alice.lib_slots.available_permits(), 4);
}

#[tokio::test]
async fn wait_finishes_only_after_the_execution_releases_its_permit() {
    let sandbox = Sandbox::new();
    let permit = sandbox.lib_slots.clone().try_acquire_owned().unwrap();
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    sandbox.tasks.spawn(async move {
        let _permit = permit;
        let _ = released.await;
    });
    sandbox.close();
    let mut waiting = Box::pin(sandbox.wait());
    assert!(futures::poll!(&mut waiting).is_pending());
    assert_eq!(sandbox.lib_slots.available_permits(), 3);
    release.send(()).unwrap();
    waiting.await.unwrap();
    assert!(sandbox.tasks.is_empty());
    assert_eq!(sandbox.lib_slots.available_permits(), 4);
}

#[tokio::test(start_paused = true)]
async fn a_shutdown_deadline_preserves_unfinished_execution_ownership() {
    let sandbox = Sandbox::new();
    let permit = sandbox.lib_slots.clone().try_acquire_owned().unwrap();
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    sandbox.tasks.spawn(async move {
        let _permit = permit;
        let _ = released.await;
    });
    sandbox.close();
    assert!(matches!(sandbox.wait().await, Err(Error::ShutdownDeadline)));
    assert_eq!(sandbox.tasks.len(), 1);
    assert_eq!(sandbox.lib_slots.available_permits(), 3);
    release.send(()).unwrap();
    sandbox.wait().await.unwrap();
    assert_eq!(sandbox.lib_slots.available_permits(), 4);
}
