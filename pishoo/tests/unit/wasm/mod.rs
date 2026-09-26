// Private unit tests live under tests/ while retaining access to the frozen
// execution module. Each included file groups one set of behaviors.
use http_body_util::{Empty, Full};
use tokio::sync::{Semaphore, mpsc};

use super::*;

const READ: &[u8] = include_bytes!("../../fixtures/wasi-http-read-request-then-respond.wasm");
const EARLY: &[u8] = include_bytes!("../../fixtures/wasi-http-respond-then-read-request.wasm");
const STREAM: &[u8] =
    include_bytes!("../../fixtures/wasi-http-stream-response-until-cancelled.wasm");
const OUTGOING: &[u8] = include_bytes!("../../fixtures/wasi-http-outgoing-client.wasm");

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

include!("streaming.rs");
include!("lifecycle.rs");
include!("policy.rs");
include!("isolation.rs");
