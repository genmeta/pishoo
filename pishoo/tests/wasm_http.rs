mod common;
#[path = "common/exchange.rs"]
mod exchange;

use std::time::Duration;

use http::StatusCode;
use pishoo::{
    sandbox::{Sandbox, Usage},
    wasm::Limits,
};
use qrecovery::recv::StopSending;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const READ_THEN_RESPOND: &[u8] =
    include_bytes!("fixtures/wasi-http-read-request-then-respond.wasm");
const RESPOND_THEN_READ: &[u8] =
    include_bytes!("fixtures/wasi-http-respond-then-read-request.wasm");
fn limits() -> Limits {
    Limits {
        fuel: 100_000_000,
        memory_bytes: 64 << 20,
        deadline: Duration::from_secs(5),
    }
}
fn sandboxed_lib(bytes: &[u8]) -> Sandbox {
    let sandbox = Sandbox::new("test", Default::default()).unwrap();
    common::load_lib(&sandbox, "test", bytes).unwrap();
    sandbox
}
async fn reclaimed(sandbox: &Sandbox) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while sandbox.usage() != Usage::default() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
fn request(path: &str) -> http::Request<h3x::ArcWndBuf> {
    let request: h3x::Request<h3x::W> = exchange::request(path).into();
    request.append_trailer(
        "x-request-trailer".parse().unwrap(),
        "preserved".parse().unwrap(),
    );
    request.into()
}

#[tokio::test]
async fn streams_request_and_response_with_repeated_trailers() {
    let sandbox = sandboxed_lib(READ_THEN_RESPOND);
    let mut request = request("/api/test/read");
    request
        .body_mut()
        .write_all(b"hello-hello-hello-hello-")
        .await
        .unwrap();
    request
        .body_mut()
        .write_all(b"hello-hello-hello-hello")
        .await
        .unwrap();
    request.body_mut().shutdown().await.unwrap();
    let mut exchange = exchange::start(&sandbox, request, limits()).await;
    let mut response = exchange.response().await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let mut bytes = Vec::new();
    response.read_to_end(&mut bytes).await.unwrap();
    assert_eq!(
        response
            .trailers()
            .get_all("x-response-trailer")
            .iter()
            .collect::<Vec<_>>(),
        ["preserved", "also-preserved"]
    );
    assert_eq!(bytes, b"world-world-".repeat(64));
    exchange.result().await.unwrap();
    reclaimed(&sandbox).await;
}

#[tokio::test]
async fn returns_headers_before_upload_finishes() {
    let sandbox = sandboxed_lib(RESPOND_THEN_READ);
    let request = request("/api/test/early");
    let mut upload = request.body().clone();
    let mut exchange = exchange::start(&sandbox, request, limits()).await;
    let mut response = tokio::time::timeout(Duration::from_secs(2), exchange.response())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(
        response.headers()["x-handler-mode"],
        "respond-then-read-request"
    );
    upload.write_all(b"request-body").await.unwrap();
    upload.shutdown().await.unwrap();
    let mut bytes = Vec::new();
    response.read_to_end(&mut bytes).await.unwrap();
    assert_eq!(
        response.trailers()["x-response-trailer"],
        "request-consumed"
    );
    assert_eq!(bytes, b"request-body");
    exchange.result().await.unwrap();
    reclaimed(&sandbox).await;
}

#[tokio::test]
async fn resetting_response_stream_reclaims_guest() {
    let sandbox = sandboxed_lib(include_bytes!(
        "fixtures/wasi-http-stream-response-until-cancelled.wasm"
    ));
    let request = exchange::empty_request("/api/test/cancel").await;
    let mut exchange = exchange::start(&sandbox, request, limits()).await;
    let mut response = exchange.response().await.unwrap();
    assert_eq!(sandbox.usage().requests, 1);
    response.stop(h3x::ErrorCode::RequestCancelled.as_u64());
    assert!(exchange.result().await.is_err());
    reclaimed(&sandbox).await;
}

#[tokio::test]
async fn outgoing_http_is_denied_without_a_host_policy() {
    let sandbox = sandboxed_lib(include_bytes!("fixtures/wasi-http-outgoing-client.wasm"));
    let request = exchange::empty_request("/api/test/absent/small/normal").await;
    let mut exchange = exchange::start(&sandbox, request, limits()).await;
    assert!(
        tokio::time::timeout(Duration::from_secs(2), exchange.result())
            .await
            .unwrap()
            .is_err()
    );
    reclaimed(&sandbox).await;
}

#[tokio::test]
async fn dropping_a_pending_call_cancels_its_guest() {
    let sandbox = sandboxed_lib(READ_THEN_RESPOND);
    let exchange =
        exchange::start(&sandbox, exchange::request("/api/test/pending"), limits()).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while sandbox.usage().requests == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(exchange);
    reclaimed(&sandbox).await;
}
