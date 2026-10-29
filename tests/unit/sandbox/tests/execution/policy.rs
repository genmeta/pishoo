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
        outgoing: Outgoing::Denied,
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

#[test]
fn wasi_http_rejects_conflicting_lengths_and_handles_long_unicode_paths() {
    use wasmtime::component::Resource;
    use wasmtime_wasi_http::{
        WasiHttpCtxView,
        p2::bindings::http::types::{HostFields, HostOutgoingRequest},
    };
    let mut table = ResourceTable::new();
    let mut http = WasiHttpCtx::new();
    let mut outgoing = Outgoing::Denied;
    let mut view = WasiHttpCtxView {
        ctx: &mut http,
        table: &mut table,
        hooks: &mut outgoing,
    };
    assert!(
        HostFields::from_list(
            &mut view,
            vec![
                ("content-length".into(), b"1".to_vec()),
                ("content-length".into(), b"2".to_vec()),
            ]
        )
        .is_err()
    );
    let fields = HostFields::new(&mut view).unwrap();
    let request = HostOutgoingRequest::new(&mut view, fields).unwrap();
    // The query offset wraps into the UTF-8 character in vulnerable http versions.
    let path = format!("/é{}?query=1", "a".repeat(65_535));
    let _ = HostOutgoingRequest::set_path_with_query(
        &mut view,
        Resource::new_borrow(request.rep()),
        Some(path),
    )
    .unwrap();
    assert!(
        HostOutgoingRequest::set_path_with_query(
            &mut view,
            Resource::new_borrow(request.rep()),
            Some("/normal?query=1".into()),
        )
        .unwrap()
        .is_ok()
    );
    HostOutgoingRequest::drop(&mut view, request).unwrap();
}
