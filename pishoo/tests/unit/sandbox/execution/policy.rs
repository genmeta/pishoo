use super::*;

#[tokio::test]
async fn guest_http_outgoing_is_denied() {
    let directory = tempfile::tempdir().unwrap();
    let lib = load(OUTGOING, directory.path());
    let tasks = TaskTracker::new();
    let result = invoke(lib, &tasks)
        .await
        .execute(request("/absent/small/normal?probe=1", empty()))
        .await;
    assert!(result.is_err());
    reaped(&tasks).await;
}

#[tokio::test]
async fn identity_signatures_enforce_bounds_and_handshake_scope() {
    use identity::pishoo::identity::signatures::{Host, SignError, VerifyError};
    let mut store = StoreData {
        table: ResourceTable::new(),
        wasi: WasiCtx::builder().build(),
        http: WasiHttpCtx::new(),
        memory: StoreLimits::default(),
        deny_outgoing: DenyOutgoing,
        local: authority("alice.dhttp.net"),
        remote: None,
    };
    let signature = store.sign(b"message".to_vec()).await.unwrap();
    assert!(
        store
            .verify(signature.clone(), b"message".to_vec(), "alice".into())
            .await
            .unwrap()
    );
    assert!(
        !store
            .verify(signature, b"changed".to_vec(), "alice.dhttp.net".into())
            .await
            .unwrap()
    );
    assert!(matches!(
        store.sign(vec![0; (1024 * 1024) + 1]).await,
        Err(SignError::InputTooLarge)
    ));
    assert!(matches!(
        store.verify(vec![0; 8193], vec![], "alice".into()).await,
        Err(VerifyError::InputTooLarge)
    ));
    assert!(matches!(
        store.verify(vec![], vec![], "../invalid".into()).await,
        Err(VerifyError::InvalidIdentity)
    ));
    assert!(matches!(
        store.verify(vec![], vec![], "bob".into()).await,
        Err(VerifyError::Unavailable)
    ));
}
