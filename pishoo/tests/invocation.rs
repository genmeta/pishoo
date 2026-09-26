mod common;
#[path = "common/h3.rs"]
mod h3;

use std::{sync::Arc, time::Duration};

use h3x::{ArcWndBuf, ReadRequest, ReadResponse, WriteRequest};
use http::{Method, StatusCode};
use pishoo::{
    outgoing::AuthorizedOutgoing,
    sandbox::{Sandbox, Usage},
    wasm::{Invocation, Limits, WasmLib},
};
use qrecovery::{recv::StopSending, send::CancelStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use wasmtime_wasi::filesystem::WasiFilesystemCtx;

fn limits() -> Limits {
    Limits {
        fuel: 100_000_000,
        memory_bytes: 64 << 20,
        deadline: Duration::from_secs(5),
    }
}
fn request(path: &str) -> h3x::Request<h3x::W> {
    let request: h3x::Request<h3x::W> = http::Request::post(format!("https://example.com{path}"))
        .body(ArcWndBuf::new(8))
        .unwrap()
        .into();
    request.append_trailer(
        "x-request-trailer".parse().unwrap(),
        "preserved".parse().unwrap(),
    );
    request
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

#[tokio::test]
async fn h3_headers_arrive_before_upload_finishes_and_trailers_survive() {
    let sandbox = Sandbox::new("alice", Default::default()).unwrap();
    common::load_lib(
        &sandbox,
        "test",
        include_bytes!("fixtures/wasi-http-respond-then-read-request.wasm"),
    )
    .unwrap();
    let (client, server) = h3::connection_pair();
    let serving = async {
        let (ws, rs) = server.accept_bi().await.unwrap();
        let request = rs.read_request(server.qpack().clone()).await.unwrap();
        sandbox
            .handle_stream(ws, request, server.qpack().clone(), limits())
            .await
            .unwrap();
    };
    let client_side = async {
        let (ws, rs) = client.open_bi().await.unwrap();
        let mut request = request("/api/test/early");
        let writing = ws.write_request(request.clone(), client.qpack().clone());
        let receiving = async {
            let mut response = rs
                .read_response(Method::POST, client.qpack().clone())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::CREATED);
            // The upload starts only after response headers have arrived.
            request.write_all(b"streamed upload").await.unwrap();
            request.shutdown().await.unwrap();
            let mut bytes = Vec::new();
            response.read_to_end(&mut bytes).await.unwrap();
            assert_eq!(bytes, b"streamed upload");
            assert_eq!(
                response.trailers()["x-response-trailer"],
                "request-consumed"
            );
        };
        let (result, ()) = tokio::join!(writing, receiving);
        result.unwrap();
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(serving, client_side);
    })
    .await
    .unwrap();
    reclaimed(&sandbox).await;
}

#[tokio::test]
async fn direct_invocations_share_compiled_code_and_preserve_repeated_trailers() {
    let bytes = common::component(
        include_bytes!("fixtures/wasi-http-read-request-then-respond.wasm"),
        &[("POST", "/read")],
    );
    let lib = Arc::new(WasmLib::new("test", &bytes).unwrap());
    let (client, server) = h3::connection_pair();
    for remote in ["bob", "carol"] {
        let serving = async {
            let (ws, rs) = server.accept_bi().await.unwrap();
            let request = rs.read_request(server.qpack().clone()).await.unwrap();
            let invocation = Invocation::new(
                lib.clone(),
                Arc::from("alice"),
                Arc::from(remote),
                WasiFilesystemCtx::default(),
                AuthorizedOutgoing::denied(CancellationToken::new()),
                limits(),
                server.qpack().clone(),
            )
            .unwrap();
            assert_eq!(invocation.remote(), remote);
            invocation.handle(ws, request).await.unwrap();
        };
        let client_side = async {
            let (ws, rs) = client.open_bi().await.unwrap();
            let mut request = request("/read");
            let writing = ws.write_request(request.clone(), client.qpack().clone());
            let uploading = async {
                request
                    .write_all(b"hello-hello-hello-hello-hello-hello-hello-hello")
                    .await
                    .unwrap();
                request.shutdown().await.unwrap();
            };
            let reading = async {
                let mut response = rs
                    .read_response(Method::POST, client.qpack().clone())
                    .await
                    .unwrap();
                let mut bytes = Vec::new();
                response.read_to_end(&mut bytes).await.unwrap();
                assert_eq!(bytes, b"world-world-".repeat(64));
                assert_eq!(
                    response
                        .trailers()
                        .get_all("x-response-trailer")
                        .iter()
                        .collect::<Vec<_>>(),
                    ["preserved", "also-preserved"]
                );
            };
            let (result, (), ()) = tokio::join!(writing, uploading, reading);
            result.unwrap();
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(serving, client_side);
        })
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn peer_reset_and_stalled_output_release_budget() {
    for reset in [true, false] {
        let sandbox = Sandbox::new("alice", Default::default()).unwrap();
        common::load_lib(
            &sandbox,
            "test",
            include_bytes!("fixtures/wasi-http-stream-response-until-cancelled.wasm"),
        )
        .unwrap();
        let (client, server) = h3::connection_pair();
        let serving = async {
            let (ws, rs) = server.accept_bi().await.unwrap();
            let request = rs.read_request(server.qpack().clone()).await.unwrap();
            let mut limits = limits();
            limits.deadline = Duration::from_millis(500);
            assert!(
                sandbox
                    .handle_stream(ws, request, server.qpack().clone(), limits)
                    .await
                    .is_err()
            );
        };
        let client_side = async {
            let (ws, rs) = client.open_bi().await.unwrap();
            let mut request = request("/api/test/cancel");
            request.shutdown().await.unwrap();
            ws.write_request(request, client.qpack().clone())
                .await
                .unwrap();
            let mut response = rs
                .read_response(Method::POST, client.qpack().clone())
                .await
                .unwrap();
            if reset {
                response.stop(h3x::ErrorCode::RequestCancelled.as_u64());
            } else {
                tokio::time::sleep(Duration::from_millis(700)).await;
                let mut bytes = Vec::new();
                assert!(response.read_to_end(&mut bytes).await.is_err());
            }
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(serving, client_side);
        })
        .await
        .unwrap();
        reclaimed(&sandbox).await;
    }
}

#[tokio::test]
async fn input_error_resets_response_and_releases_budget() {
    let sandbox = Sandbox::new("alice", Default::default()).unwrap();
    common::load_lib(
        &sandbox,
        "test",
        include_bytes!("fixtures/wasi-http-respond-then-read-request.wasm"),
    )
    .unwrap();
    let (client, server) = h3::connection_pair();
    let serving = async {
        let (ws, rs) = server.accept_bi().await.unwrap();
        let request = rs.read_request(server.qpack().clone()).await.unwrap();
        assert!(
            sandbox
                .handle_stream(ws, request, server.qpack().clone(), limits())
                .await
                .is_err()
        );
    };
    let client_side = async {
        let (ws, rs) = client.open_bi().await.unwrap();
        let mut request = request("/api/test/early");
        let writing = ws.write_request(request.clone(), client.qpack().clone());
        let reading = async {
            let mut response = rs
                .read_response(Method::POST, client.qpack().clone())
                .await
                .unwrap();
            request.cancel(h3x::ErrorCode::RequestCancelled.as_u64());
            let mut bytes = Vec::new();
            assert!(response.read_to_end(&mut bytes).await.is_err());
        };
        let (result, ()) = tokio::join!(writing, reading);
        assert!(result.is_err());
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(serving, client_side);
    })
    .await
    .unwrap();
    reclaimed(&sandbox).await;
}

#[tokio::test]
async fn dropping_handler_resets_pending_body_and_reclaims_guest() {
    let sandbox = Sandbox::new("alice", Default::default()).unwrap();
    common::load_lib(
        &sandbox,
        "test",
        include_bytes!("fixtures/wasi-http-respond-then-read-request.wasm"),
    )
    .unwrap();
    let (client, server) = h3::connection_pair();
    let (drop_tx, drop_rx) = tokio::sync::oneshot::channel();
    let serving = async {
        let (ws, rs) = server.accept_bi().await.unwrap();
        let request = rs.read_request(server.qpack().clone()).await.unwrap();
        let task = tokio::spawn({
            let sandbox = sandbox.clone();
            let qpack = server.qpack().clone();
            async move { sandbox.handle_stream(ws, request, qpack, limits()).await }
        });
        drop_rx.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
    };
    let client_side = async {
        let (ws, rs) = client.open_bi().await.unwrap();
        let mut request = request("/api/test/early");
        let writing = ws.write_request(request.clone(), client.qpack().clone());
        let reading = async {
            let mut response = rs
                .read_response(Method::POST, client.qpack().clone())
                .await
                .unwrap();
            drop_tx.send(()).unwrap();
            let mut bytes = Vec::new();
            assert!(response.read_to_end(&mut bytes).await.is_err());
            // Wake the upload producer: the in-memory transport reports peer
            // STOP_SENDING on its next write, rather than via a QUIC driver.
            request.write_all(b"next").await.unwrap();
            request.shutdown().await.unwrap();
        };
        let (result, ()) = tokio::join!(writing, reading);
        assert!(result.is_err());
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(serving, client_side);
    })
    .await
    .unwrap();
    reclaimed(&sandbox).await;
}
