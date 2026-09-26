use http_body_util::{Empty, Full};
use tokio::sync::{Semaphore, mpsc};

use super::*;

const READ: &[u8] = include_bytes!("../../tests/fixtures/wasi-http-read-request-then-respond.wasm");
const EARLY: &[u8] =
    include_bytes!("../../tests/fixtures/wasi-http-respond-then-read-request.wasm");
const STREAM: &[u8] =
    include_bytes!("../../tests/fixtures/wasi-http-stream-response-until-cancelled.wasm");
const OUTGOING: &[u8] = include_bytes!("../../tests/fixtures/wasi-http-outgoing-client.wasm");

fn component(bytes: &[u8]) -> Vec<u8> {
    fn leb(mut n: usize, output: &mut Vec<u8>) {
        loop {
            let byte = (n & 127) as u8;
            n >>= 7;
            output.push(byte | if n != 0 { 128 } else { 0 });
            if n == 0 {
                break;
            }
        }
    }
    let doc = br#"{"openapi":"3.1.0","info":{"title":"Test","version":"1"},"paths":{"/read":{"post":{}},"/early":{"post":{}},"/cancel":{"post":{}},"/absent/small/normal":{"post":{}}}}"#;
    let mut section = Vec::new();
    leb(b"pishoo:openapi".len(), &mut section);
    section.extend_from_slice(b"pishoo:openapi");
    section.extend_from_slice(doc);
    let mut bytes = bytes.to_vec();
    bytes.push(0);
    leb(section.len(), &mut bytes);
    bytes.extend(section);
    bytes
}

fn authority(name: &str) -> dhttp::LocalAuthority {
    let cert = rcgen::generate_simple_self_signed(vec![name.into()]).unwrap();
    dhttp::LocalAuthority::new(
        &qtls::default_provider(),
        Arc::from(name),
        vec![cert.cert.der().clone()],
        qtls::PrivateKeyDer::try_from(cert.signing_key.serialize_der()).unwrap(),
        vec![1],
    )
    .unwrap()
}

fn load(bytes: &[u8], directory: &Path, cancel: CancellationToken) -> Arc<Lib> {
    Arc::new(
        Lib::load(
            Arc::new(Runtime::new().unwrap()),
            "test".into(),
            &component(bytes),
            directory,
            LibPolicy::default(),
            cancel,
        )
        .unwrap(),
    )
}

async fn invoke(lib: Arc<Lib>, slots: &Arc<Semaphore>, tasks: &TaskTracker) -> Invocation {
    Invocation::new(
        lib,
        slots.clone().try_acquire_owned().unwrap(),
        dhttp::Endpoint::load("alice.dhttp.net").await.unwrap(),
        &dhttp::HandshakeSummary {
            alpn: None,
            local: Some(authority("alice.dhttp.net")),
            remote: None,
        },
        tasks.clone(),
    )
    .unwrap()
}

fn upload() -> (
    mpsc::Sender<std::result::Result<Frame<Bytes>, dhttp::BoxError>>,
    Body,
) {
    let (tx, rx) = mpsc::channel(1);
    let frames = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|frame| (frame, rx))
    });
    (tx, StreamBody::new(Box::pin(frames)).boxed_unsync())
}

fn request(path: &str, body: Body) -> Request<Body> {
    Request::post(format!("https://alice.dhttp.net{path}"))
        .body(body)
        .unwrap()
}

fn empty() -> Body {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed_unsync()
}

async fn reaped(tasks: &TaskTracker, slots: &Semaphore) {
    tasks.close();
    tokio::time::timeout(Duration::from_secs(5), tasks.wait())
        .await
        .unwrap();
    assert_eq!(slots.available_permits(), 4);
}

#[tokio::test]
async fn upload_and_response_stream_with_repeated_trailers() {
    let directory = tempfile::tempdir().unwrap();
    let lib = load(READ, directory.path(), CancellationToken::new());
    let slots = Arc::new(Semaphore::new(4));
    let tasks = TaskTracker::new();
    let invocation = invoke(lib, &slots, &tasks).await;
    let (tx, body) = upload();
    let producer = async move {
        tx.send(Ok(Frame::data(Bytes::from_static(
            b"hello-hello-hello-hello-",
        ))))
        .await
        .unwrap();
        tx.send(Ok(Frame::data(Bytes::from_static(
            b"hello-hello-hello-hello",
        ))))
        .await
        .unwrap();
        let mut trailers = http::HeaderMap::new();
        trailers.append("x-request-trailer", "preserved".parse().unwrap());
        tx.send(Ok(Frame::trailers(trailers))).await.unwrap();
    };
    let (response, ()) = tokio::join!(invocation.execute(request("/read", body)), producer);
    let response = response.unwrap();
    assert_eq!(response.status(), http::StatusCode::CREATED);
    let collected = response.into_body().collect().await.unwrap();
    assert_eq!(
        collected
            .trailers()
            .unwrap()
            .get_all("x-response-trailer")
            .iter()
            .collect::<Vec<_>>(),
        ["preserved", "also-preserved"]
    );
    assert_eq!(collected.to_bytes(), b"world-world-".repeat(64));
    reaped(&tasks, &slots).await;
}

#[tokio::test]
async fn response_headers_arrive_before_upload_eof() {
    let directory = tempfile::tempdir().unwrap();
    let lib = load(EARLY, directory.path(), CancellationToken::new());
    let slots = Arc::new(Semaphore::new(4));
    let tasks = TaskTracker::new();
    let invocation = invoke(lib, &slots, &tasks).await;
    let (tx, body) = upload();
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        invocation.execute(request("/early", body)),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        response.headers()["x-handler-mode"],
        "respond-then-read-request"
    );
    assert_eq!(slots.available_permits(), 3);
    let producer = async move {
        tx.send(Ok(Frame::data(Bytes::from_static(b"streamed upload"))))
            .await
            .unwrap();
        let mut trailers = http::HeaderMap::new();
        trailers.append("x-request-trailer", "preserved".parse().unwrap());
        tx.send(Ok(Frame::trailers(trailers))).await.unwrap();
    };
    let (collected, ()) = tokio::join!(response.into_body().collect(), producer);
    let collected = collected.unwrap();
    assert_eq!(
        collected.trailers().unwrap()["x-response-trailer"],
        "request-consumed"
    );
    assert_eq!(collected.to_bytes(), "streamed upload");
    reaped(&tasks, &slots).await;
}

#[tokio::test]
async fn replacing_an_unpolled_body_reaps_only_its_producer() {
    let directory = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let lib = load(STREAM, directory.path(), cancel.clone());
    let slots = Arc::new(Semaphore::new(4));
    let tasks = TaskTracker::new();
    let response = invoke(lib.clone(), &slots, &tasks)
        .await
        .execute(request("/cancel", empty()))
        .await
        .unwrap();
    let mut other = invoke(lib, &slots, &tasks)
        .await
        .execute(request("/cancel", empty()))
        .await
        .unwrap();
    let replacement = response.map(|body| {
        drop(body);
        Full::new(Bytes::from_static(b"replacement"))
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while slots.available_permits() != 3 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!cancel.is_cancelled());
    assert!(other.body_mut().frame().await.unwrap().unwrap().is_data());
    assert_eq!(
        replacement.into_body().collect().await.unwrap().to_bytes(),
        "replacement"
    );
    drop(other);
    reaped(&tasks, &slots).await;
}

#[tokio::test]
async fn dropping_pending_execute_releases_upload_and_permit() {
    let directory = tempfile::tempdir().unwrap();
    let lib = load(READ, directory.path(), CancellationToken::new());
    let slots = Arc::new(Semaphore::new(4));
    let tasks = TaskTracker::new();
    let invocation = invoke(lib, &slots, &tasks).await;
    let (tx, body) = upload();
    let execute = tokio::spawn(invocation.execute(request("/read", body)));
    tokio::task::yield_now().await;
    execute.abort();
    assert!(execute.await.unwrap_err().is_cancelled());
    reaped(&tasks, &slots).await;
    assert!(tx.is_closed());
}

#[tokio::test]
async fn upload_error_becomes_one_body_error_and_releases_permit() {
    let directory = tempfile::tempdir().unwrap();
    let lib = load(EARLY, directory.path(), CancellationToken::new());
    let slots = Arc::new(Semaphore::new(4));
    let tasks = TaskTracker::new();
    let (tx, body) = upload();
    let mut response = invoke(lib, &slots, &tasks)
        .await
        .execute(request("/early", body))
        .await
        .unwrap();
    tx.send(Err(std::io::Error::other("upload reset").into()))
        .await
        .unwrap();
    drop(tx);
    loop {
        if response.body_mut().frame().await.unwrap().is_err() {
            break;
        }
    }
    assert!(response.body_mut().frame().await.is_none());
    reaped(&tasks, &slots).await;
}

#[tokio::test(start_paused = true)]
async fn unpolled_response_has_an_independent_deadline() {
    let directory = tempfile::tempdir().unwrap();
    let lib = load(STREAM, directory.path(), CancellationToken::new());
    let slots = Arc::new(Semaphore::new(4));
    let tasks = TaskTracker::new();
    let mut response = invoke(lib, &slots, &tasks)
        .await
        .execute(request("/cancel", empty()))
        .await
        .unwrap();
    assert_eq!(slots.available_permits(), 3);
    tokio::time::advance(Duration::from_secs(31)).await;
    reaped(&tasks, &slots).await;
    let error = response.body_mut().frame().await.unwrap().unwrap_err();
    assert!(matches!(
        error.downcast_ref::<Error>(),
        Some(Error::Deadline)
    ));
    assert!(response.body_mut().frame().await.is_none());
}

#[tokio::test]
async fn four_slots_survive_version_replacement_and_cancel_together() {
    let directory = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let old = load(STREAM, directory.path(), cancel.clone());
    let new = Arc::new(
        Lib::load(
            old.runtime.clone(),
            "test".into(),
            &component(STREAM),
            directory.path(),
            LibPolicy::default(),
            cancel.clone(),
        )
        .unwrap(),
    );
    let slots = Arc::new(Semaphore::new(4));
    let tasks = TaskTracker::new();
    let mut responses = Vec::new();
    for lib in [old.clone(), new.clone(), old, new.clone()] {
        responses.push(
            invoke(lib, &slots, &tasks)
                .await
                .execute(request("/cancel", empty()))
                .await
                .unwrap(),
        );
    }
    assert_eq!(slots.available_permits(), 0);
    assert!(slots.clone().try_acquire_owned().is_err());
    cancel.cancel();
    reaped(&tasks, &slots).await;
    for mut response in responses {
        assert!(response.body_mut().frame().await.unwrap().is_err());
    }
    let result = Invocation::new(
        new,
        slots.try_acquire_owned().unwrap(),
        dhttp::Endpoint::load("alice").await.unwrap(),
        &dhttp::HandshakeSummary {
            alpn: None,
            local: Some(authority("alice.dhttp.net")),
            remote: None,
        },
        TaskTracker::new(),
    );
    assert!(matches!(result, Err(Error::Cancelled)));
}

#[tokio::test]
async fn outgoing_fixture_is_denied_by_default() {
    let directory = tempfile::tempdir().unwrap();
    let lib = load(OUTGOING, directory.path(), CancellationToken::new());
    let slots = Arc::new(Semaphore::new(4));
    let tasks = TaskTracker::new();
    let mut request = request("/absent/small/normal?probe=1", empty());
    request
        .headers_mut()
        .insert("pishoo-client-identity", "alice.dhttp.net".parse().unwrap());
    let result = invoke(lib, &slots, &tasks).await.execute(request).await;
    assert!(result.is_err());
    reaped(&tasks, &slots).await;
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

#[test]
fn memory_limit_counts_all_memories_and_rolls_back_failed_growth() {
    let mut limit = MemoryLimits {
        base: StoreLimitsBuilder::new().table_elements(100_000).build(),
        used: 0,
        pending: 0,
    };
    assert!(limit.memory_growing(0, 32 << 20, None).unwrap());
    assert!(limit.memory_growing(0, 32 << 20, None).unwrap());
    limit
        .memory_grow_failed(wasmtime::Error::msg("allocation failed"))
        .unwrap();
    assert_eq!(limit.used, 32 << 20);
    assert!(limit.memory_growing(0, 32 << 20, None).unwrap());
    assert!(!limit.memory_growing(0, 65536, None).unwrap());
    assert!(!limit.table_growing(0, 100_001, None).unwrap());
}

#[tokio::test]
async fn filesystem_grants_are_private_read_only_and_hold_the_open_directory() {
    use wasmtime::component::Resource;
    use wasmtime_wasi::{
        filesystem::WasiFilesystemCtxView,
        p2::bindings::filesystem::{
            preopens::Host,
            types::{DescriptorFlags, HostDescriptor, OpenFlags, PathFlags},
        },
    };
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("first")).unwrap();
    std::fs::create_dir(root.path().join("second")).unwrap();
    std::fs::write(root.path().join("first/value"), "first").unwrap();
    std::fs::write(root.path().join("second/value"), "secret sibling").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        root.path().join("second/value"),
        root.path().join("first/link"),
    )
    .unwrap();
    let runtime = Arc::new(Runtime::new().unwrap());
    let lib = Lib::load(
        runtime,
        "test".into(),
        &component(READ),
        &root.path().join("first"),
        LibPolicy {
            data_write: false,
            ..LibPolicy::default()
        },
        CancellationToken::new(),
    )
    .unwrap();
    std::fs::rename(root.path().join("first"), root.path().join("saved")).unwrap();
    std::fs::create_dir(root.path().join("first")).unwrap();
    std::fs::write(root.path().join("first/value"), "replacement").unwrap();
    let mut table = ResourceTable::new();
    let mut filesystem = lib.filesystem.clone();
    let mut view = WasiFilesystemCtxView {
        ctx: &mut filesystem,
        table: &mut table,
    };
    let dirs = Host::get_directories(&mut view).unwrap();
    assert_eq!(dirs.len(), 1);
    assert_eq!(dirs[0].1, "/data");
    let descriptor = dirs[0].0.rep();
    let file = view
        .open_at(
            Resource::new_borrow(descriptor),
            PathFlags::empty(),
            "value".into(),
            OpenFlags::empty(),
            DescriptorFlags::READ,
        )
        .await
        .unwrap();
    assert_eq!(view.read(file, 100, 0).await.unwrap().0, b"first");
    assert!(
        view.open_at(
            Resource::new_borrow(descriptor),
            PathFlags::empty(),
            "value".into(),
            OpenFlags::empty(),
            DescriptorFlags::WRITE
        )
        .await
        .is_err()
    );
    for path in ["../second/value", "/second/value", "link"] {
        assert!(
            view.open_at(
                Resource::new_borrow(descriptor),
                PathFlags::SYMLINK_FOLLOW,
                path.into(),
                OpenFlags::empty(),
                DescriptorFlags::READ
            )
            .await
            .is_err()
        );
    }
}

#[tokio::test]
async fn response_body_waits_for_guest_before_final_trailers_and_disarms_on_eof() {
    let cancel = CancellationToken::new();
    let (tx, rx) = oneshot::channel();
    let guest = tokio::spawn(async {
        rx.await.unwrap();
        Ok(())
    });
    let mut trailers = http::HeaderMap::new();
    trailers.append("x-end", "first".parse().unwrap());
    trailers.append("x-end", "second".parse().unwrap());
    let frames = futures::stream::iter([
        Ok(Frame::data(Bytes::from_static(b"data"))),
        Ok(Frame::trailers(trailers)),
    ]);
    let mut body = LibResponseBody::Reading {
        inner: StreamBody::new(frames).boxed_unsync(),
        guest: Some(guest),
        cancel_on_drop: cancel.clone().drop_guard(),
    };
    assert_eq!(
        body.frame().await.unwrap().unwrap().into_data().unwrap(),
        "data"
    );
    assert!(futures::poll!(body.frame()).is_pending());
    assert!(matches!(body, LibResponseBody::Waiting { .. }));
    tx.send(()).unwrap();
    assert_eq!(
        body.frame()
            .await
            .unwrap()
            .unwrap()
            .into_trailers()
            .unwrap()
            .get_all("x-end")
            .iter()
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
    assert!(body.frame().await.is_none());
    drop(body);
    assert!(!cancel.is_cancelled());
}

#[tokio::test]
async fn identity_signatures_require_capabilities_and_enforce_bounds() {
    use identity::pishoo::identity::signatures::{Host, SignError, VerifyError};
    let slots = Arc::new(Semaphore::new(4));
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
        permit: slots.clone().try_acquire_owned().unwrap(),
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
    assert_eq!(slots.available_permits(), 4);
}

#[tokio::test]
async fn guest_join_failure_is_reported_once_and_cancels_producer() {
    let cancel = CancellationToken::new();
    let guest = tokio::spawn(async {
        panic!("guest task panic");
        #[allow(unreachable_code)]
        Ok(())
    });
    let mut body = LibResponseBody::Reading {
        inner: Empty::<Bytes>::new()
            .map_err(|never| match never {})
            .boxed_unsync(),
        guest: Some(guest),
        cancel_on_drop: cancel.clone().drop_guard(),
    };
    assert!(matches!(body.frame().await.unwrap(), Err(Error::Task(_))));
    assert!(body.frame().await.is_none());
    assert!(cancel.is_cancelled());
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

#[tokio::test]
async fn cpu_only_guest_yields_for_cancellation_and_is_reaped() {
    let bytes = wat::parse_str(
        r#"
        (component
            (import "wasi:http/types@0.2.12" (instance $types
                (export "incoming-request" (type (sub resource)))
                (export "response-outparam" (type (sub resource)))
            ))
            (alias export $types "incoming-request" (type $request))
            (alias export $types "response-outparam" (type $response))
            (core module $guest
                (func (export "handle") (param i32 i32)
                    (loop $forever br $forever)))
            (core instance $guest (instantiate $guest))
            (func $handle
                (param "request" (own $request))
                (param "response-out" (own $response))
                (canon lift (core func $guest "handle")))
            (instance $handler (export "handle" (func $handle)))
            (export "wasi:http/incoming-handler@0.2.12" (instance $handler))
        )
    "#,
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let lib = load(&bytes, directory.path(), cancel.clone());
    let slots = Arc::new(Semaphore::new(4));
    let tasks = TaskTracker::new();
    let invocation = invoke(lib.clone(), &slots, &tasks).await;
    let cancelling = async {
        tokio::time::sleep(Duration::from_millis(1)).await;
        cancel.cancel();
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(invocation.execute(request("/read", empty())), cancelling)
    })
    .await
    .unwrap();
    assert!(matches!(result, Err(Error::Cancelled)));
    reaped(&tasks, &slots).await;
}

#[test]
fn real_store_cannot_allocate_more_than_the_combined_memory_limit() {
    let engine = Engine::default();
    let mut store = Store::new(
        &engine,
        MemoryLimits {
            base: StoreLimitsBuilder::new().memory_size(64 << 20).build(),
            used: 0,
            pending: 0,
        },
    );
    store.limiter(|limits| limits);
    let memory = wasmtime::MemoryType::new(512, None);
    let _first = wasmtime::Memory::new(&mut store, memory.clone()).unwrap();
    let _second = wasmtime::Memory::new(&mut store, memory).unwrap();
    assert_eq!(store.data().used, 64 << 20);
    assert!(wasmtime::Memory::new(&mut store, wasmtime::MemoryType::new(1, None)).is_err());
}

#[test]
fn lib_ids_follow_the_deployment_grammar() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = Arc::new(Runtime::new().unwrap());
    for id in [
        "",
        "Upper",
        "with_underscore",
        "1number",
        "../other",
        &"a".repeat(64),
    ] {
        let error = Lib::load(
            runtime.clone(),
            id.into(),
            &[],
            directory.path(),
            LibPolicy::default(),
            CancellationToken::new(),
        )
        .err()
        .unwrap();
        assert!(matches!(error, Error::InvalidComponent(message) if message == "invalid Lib id"));
    }
}

#[cfg(unix)]
#[test]
fn data_directory_symlinks_cannot_grant_sibling_or_identity_files() {
    let root = tempfile::tempdir().unwrap();
    let runtime = Arc::new(Runtime::new().unwrap());
    let bytes = component(READ);
    for target in ["ssl", "db", "sibling"] {
        let target = root.path().join(target);
        std::fs::create_dir(&target).unwrap();
        let data = root.path().join("data");
        std::os::unix::fs::symlink(&target, &data).unwrap();
        let error = Lib::load(
            runtime.clone(),
            "test".into(),
            &bytes,
            &data,
            LibPolicy::default(),
            CancellationToken::new(),
        )
        .err()
        .unwrap();
        assert!(matches!(error, Error::InvalidComponent(_)));
        std::fs::remove_file(data).unwrap();
    }
}
