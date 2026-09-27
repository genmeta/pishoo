use super::*;

#[tokio::test]
async fn outgoing_fixture_is_denied_by_default() {
    let directory = tempfile::tempdir().unwrap();
    let lib = load(OUTGOING, directory.path(), CancellationToken::new());
    let tasks = TaskTracker::new();
    let mut request = request("/absent/small/normal?probe=1", empty());
    request
        .headers_mut()
        .insert("pishoo-client-identity", "alice.dhttp.net".parse().unwrap());
    let result = invoke(lib, &tasks).await.execute(request).await;
    assert!(result.is_err());
    reaped(&tasks).await;
}

#[test]
fn target_rules_reject_management_aliases_and_prefix_confusion() {
    let policy = LibPolicy {
        outgoing: vec![OutgoingRule {
            methods: vec![Method::GET],
            origin: "https://bob.dhttp.net".parse().unwrap(),
            path_prefix: "/".into(),
        }],
        ..LibPolicy::default()
    };
    assert!(outgoing_allowed(
        &policy,
        &Method::GET,
        &"https://bob.dhttp.net/api/weather?x=1".parse().unwrap()
    ));
    for path in [
        "/acl",
        "/acl/reviews/live",
        "/contact",
        "/contacts",
        "/%61cl/review",
        "/api/../acl",
        "/api/%2e%2e/acl",
        "/api%2facl",
        "/%2561cl",
        "/workspace-api/context",
    ] {
        assert!(
            !outgoing_allowed(
                &policy,
                &Method::GET,
                &format!("https://bob.dhttp.net{path}").parse().unwrap()
            ),
            "{path}"
        );
    }
    assert!(!outgoing_allowed(
        &policy,
        &Method::POST,
        &"https://bob.dhttp.net/api/weather".parse().unwrap()
    ));
    assert!(!outgoing_allowed(
        &policy,
        &Method::GET,
        &"https://carol.dhttp.net/api/weather".parse().unwrap()
    ));
    let policy = LibPolicy {
        outgoing: vec![OutgoingRule {
            methods: vec![Method::GET],
            origin: "https://bob.dhttp.net".parse().unwrap(),
            path_prefix: "/api".into(),
        }],
        ..LibPolicy::default()
    };
    assert!(!outgoing_allowed(
        &policy,
        &Method::GET,
        &"https://bob.dhttp.net/apiculture".parse().unwrap()
    ));
}

#[tokio::test]
async fn identity_signatures_require_capabilities_and_enforce_bounds() {
    use identity::pishoo::identity::signatures::{Host, SignError, VerifyError};
    let mut store = StoreData {
        table: ResourceTable::new(),
        wasi: WasiCtx::builder().build(),
        http: WasiHttpCtx::new(),
        memory: MemoryLimits {
            base: StoreLimitsBuilder::new().build(),
            used: 0,
            pending: 0,
        },
        outgoing: HostOutgoing {
            endpoint: None,
            policy: LibPolicy::default(),
            remaining_requests: 16,
            children: TaskTracker::new(),
            cancel: CancellationToken::new(),
        },
        local: authority("alice.dhttp.net"),
        remote: None,
        policy: LibPolicy::default(),
    };
    assert!(matches!(
        store.sign(b"message".to_vec()).await,
        Err(SignError::Denied)
    ));
    assert!(matches!(
        store.verify(vec![], vec![], "alice".into()).await,
        Err(VerifyError::Unavailable)
    ));
    store.policy.sign = true;
    store.policy.verify = true;
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
    assert_eq!(store.outgoing.remaining_requests, 16);
    store.outgoing.cancel.cancel();
    assert!(matches!(
        store.sign(vec![]).await,
        Err(SignError::Unavailable)
    ));
    assert!(matches!(
        store.verify(vec![], vec![], "alice".into()).await,
        Err(VerifyError::Unavailable)
    ));
    drop(store);
}

#[tokio::test]
async fn host_outgoing_requires_identity_and_retains_the_fixed_buffer_limits() {
    let mut host = HostOutgoing {
        endpoint: None,
        policy: LibPolicy {
            outgoing: vec![OutgoingRule {
                methods: vec![Method::GET],
                origin: "https://bob.dhttp.net".parse().unwrap(),
                path_prefix: "/api".into(),
            }],
            ..LibPolicy::default()
        },
        remaining_requests: 16,
        children: TaskTracker::new(),
        cancel: CancellationToken::new(),
    };
    let make_request = || {
        Request::get("https://bob.dhttp.net/api/data")
            .body(
                Empty::<Bytes>::new()
                    .map_err(|never| match never {})
                    .boxed_unsync(),
            )
            .unwrap()
    };
    let config = || OutgoingRequestConfig {
        use_tls: true,
        connect_timeout: Duration::from_secs(1),
        first_byte_timeout: Duration::from_secs(1),
        between_bytes_timeout: Duration::from_secs(1),
    };
    assert!(host.send_request(make_request(), config()).is_err());
    assert_eq!(host.remaining_requests, 16);
    host.endpoint = Some(dhttp::Endpoint::load("alice").await.unwrap());
    host.remaining_requests = 0;
    assert!(host.send_request(make_request(), config()).is_err());
    host.remaining_requests = 16;
    host.cancel.cancel();
    assert!(host.send_request(make_request(), config()).is_err());
    assert_eq!(host.remaining_requests, 16);
    assert!(host.children.is_empty());
    assert_eq!(host.outgoing_body_buffer_chunks(), 1);
    assert_eq!(host.outgoing_body_chunk_size(), 16 * 1024);
}
