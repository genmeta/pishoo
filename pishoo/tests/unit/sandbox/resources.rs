use super::*;

#[test]
fn identities_share_runtime_and_close_only_their_own_tasks() {
    let runtime = Arc::new(Runtime::new().unwrap());
    let mut alice = Sandbox::new(runtime.clone());
    let alice_tasks = alice.tasks.clone();
    let bob = Sandbox::new(runtime);
    assert!(Arc::ptr_eq(&alice.runtime, &bob.runtime));

    alice.close();
    alice.close();
    assert!(alice_tasks.is_closed());
    assert!(!bob.tasks.is_closed());
}

#[tokio::test]
async fn wait_finishes_only_after_the_execution_returns() {
    let mut sandbox = Sandbox::new(Arc::new(Runtime::new().unwrap()));
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    sandbox.tasks.spawn(async move {
        let _ = released.await;
    });
    sandbox.close();
    let mut waiting = Box::pin(sandbox.wait());
    assert!(futures::poll!(&mut waiting).is_pending());
    assert_eq!(sandbox.tasks.len(), 1);
    release.send(()).unwrap();
    waiting.await.unwrap();
    assert!(sandbox.tasks.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_shutdown_deadline_preserves_unfinished_execution_ownership() {
    let mut sandbox = Sandbox::new(Arc::new(Runtime::new().unwrap()));
    let (finished, completion) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    sandbox.tasks.spawn(async move {
        let _ = released.await;
        finished.send(()).unwrap();
    });
    sandbox.close();
    assert!(matches!(sandbox.wait().await, Err(Error::ShutdownDeadline)));
    assert_eq!(sandbox.tasks.len(), 1);
    release.send(()).unwrap();
    sandbox.wait().await.unwrap();
    completion.await.unwrap();
    assert!(sandbox.tasks.is_empty());
}
