use std::{convert::Infallible, time::Duration};

use bytes::Bytes;
use futures::stream;
use http::{HeaderMap, HeaderValue, Method, Request, StatusCode};
use http_body::Frame;
use http_body_util::{BodyExt, StreamBody};
use pishoo::wasm::{Limits, WasmHttpApp};
use tokio::sync::mpsc;
use wasmtime_wasi_http::p2::bindings::http::types::Scheme;

const READ_THEN_RESPOND: &[u8] =
    include_bytes!("fixtures/wasi-http-read-request-then-respond.wasm");
const RESPOND_THEN_READ: &[u8] =
    include_bytes!("fixtures/wasi-http-respond-then-read-request.wasm");

fn limits() -> Limits {
    Limits {
        fuel: 100_000_000,
        memory_bytes: 64 * 1024 * 1024,
        deadline: Duration::from_secs(5),
    }
}

fn trailers() -> HeaderMap {
    let mut trailers = HeaderMap::new();
    trailers.insert("x-request-trailer", HeaderValue::from_static("preserved"));
    trailers
}

#[tokio::test]
async fn streams_request_and_response_with_repeated_trailers() {
    let app = WasmHttpApp::from_bytes(READ_THEN_RESPOND).unwrap();
    let frames = stream::iter([
        Ok::<_, Infallible>(Frame::data(Bytes::from_static(b"hello-hello-hello-hello-"))),
        Ok(Frame::data(Bytes::from_static(b"hello-hello-hello-hello"))),
        Ok(Frame::trailers(trailers())),
    ]);
    let request = Request::builder()
        .method(Method::POST)
        .uri("https://example.com/read")
        .body(StreamBody::new(frames))
        .unwrap();
    let response = app.invoke(request, Scheme::Https, limits()).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let collected = response.into_body().collect().await.unwrap();
    let trailers = collected.trailers().unwrap();
    let values: Vec<_> = trailers.get_all("x-response-trailer").iter().collect();
    assert_eq!(values, ["preserved", "also-preserved"]);
    assert_eq!(
        collected.to_bytes(),
        Bytes::from(b"world-world-".repeat(64))
    );
}

#[tokio::test]
async fn returns_headers_before_upload_finishes() {
    let app = WasmHttpApp::from_bytes(RESPOND_THEN_READ).unwrap();
    let (tx, rx) = mpsc::channel::<Result<Frame<Bytes>, Infallible>>(1);
    let frames = stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|frame| (frame, rx))
    });
    let request = Request::builder()
        .method(Method::POST)
        .uri("https://example.com/early")
        .body(StreamBody::new(frames))
        .unwrap();
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        app.invoke(request, Scheme::Https, limits()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(
        response.headers()["x-handler-mode"],
        "respond-then-read-request"
    );
    tx.send(Ok(Frame::data(Bytes::from_static(b"request-body"))))
        .await
        .unwrap();
    tx.send(Ok(Frame::trailers(trailers()))).await.unwrap();
    drop(tx);
    let collected = tokio::time::timeout(Duration::from_secs(2), response.into_body().collect())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        collected.trailers().unwrap()["x-response-trailer"],
        "request-consumed"
    );
    assert_eq!(collected.to_bytes(), Bytes::from_static(b"request-body"));
}

#[tokio::test]
async fn dropping_response_body_reclaims_guest() {
    let app = WasmHttpApp::from_bytes(include_bytes!(
        "fixtures/wasi-http-stream-response-until-cancelled.wasm"
    ))
    .unwrap();
    let request = Request::builder()
        .method(Method::POST)
        .uri("https://example.com/cancel")
        .body(http_body_util::Empty::<Bytes>::new())
        .unwrap();
    let response = app.invoke(request, Scheme::Https, limits()).await.unwrap();
    assert_eq!(app.active_guest_tasks(), 1);
    drop(response);
    tokio::time::timeout(Duration::from_secs(2), async {
        while app.active_guest_tasks() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn outgoing_http_is_denied_without_a_host_policy() {
    let app =
        WasmHttpApp::from_bytes(include_bytes!("fixtures/wasi-http-outgoing-client.wasm")).unwrap();
    let request = Request::builder()
        .method(Method::POST)
        .uri("https://example.com/absent/small/normal")
        .body(http_body_util::Empty::<Bytes>::new())
        .unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        app.invoke(request, Scheme::Https, limits()),
    )
    .await
    .unwrap();
    assert!(result.is_err());
    tokio::time::timeout(Duration::from_secs(2), async {
        while app.active_guest_tasks() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn dropping_a_pending_call_cancels_its_guest() {
    let app = WasmHttpApp::from_bytes(READ_THEN_RESPOND).unwrap();
    let (_tx, rx) = mpsc::channel::<Result<Frame<Bytes>, Infallible>>(1);
    let frames = stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|frame| (frame, rx))
    });
    let request = Request::builder()
        .method(Method::POST)
        .uri("https://example.com/pending")
        .body(StreamBody::new(frames))
        .unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            app.invoke(request, Scheme::Https, limits())
        )
        .await
        .is_err()
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        while app.active_guest_tasks() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
