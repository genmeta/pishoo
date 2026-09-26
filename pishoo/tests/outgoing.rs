mod common;
#[path = "common/exchange.rs"]
mod exchange;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use bytes::Bytes;
use http::{Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use pishoo::{
    outgoing::{OutgoingContext, OutgoingHandler},
    sandbox::{Sandbox, Usage},
    wasm::Limits,
};
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;
use wasmtime_wasi_http::p2::{
    HttpResult,
    bindings::http::types::ErrorCode,
    body::HyperOutgoingBody,
    types::{HostFutureIncomingResponse, IncomingResponse, OutgoingRequestConfig},
};

struct MockHost {
    calls: Mutex<Vec<String>>,
    allow: bool,
}
impl OutgoingHandler for MockHost {
    fn send_request(
        &self,
        source: &str,
        request: Request<HyperOutgoingBody>,
        config: OutgoingRequestConfig,
        cancel: CancellationToken,
    ) -> HttpResult<HostFutureIncomingResponse> {
        let first = self.calls.lock().unwrap().is_empty();
        self.calls.lock().unwrap().push(source.to_owned());
        assert_eq!(request.uri().path(), "/absent/small/normal");
        assert_eq!(
            request.uri().query(),
            if first { Some("probe=1") } else { None }
        );
        assert!(!cancel.is_cancelled());
        assert_eq!(
            request.uri().authority().unwrap().as_str(),
            "example.com:443"
        );
        assert_eq!(request.headers()["x-guest"], "wasm");
        if !self.allow {
            return Err(ErrorCode::HttpRequestDenied.into());
        }
        let body = Full::new(Bytes::from_static(b"response"))
            .map_err(|never| match never {})
            .boxed_unsync();
        Ok(HostFutureIncomingResponse::ready(Ok(Ok(
            IncomingResponse {
                resp: Response::builder()
                    .header("x-server", "h3x")
                    .body(body)
                    .unwrap(),
                worker: None,
                between_bytes_timeout: config.between_bytes_timeout,
            },
        ))))
    }
}

#[tokio::test]
async fn outgoing_requires_same_trusted_identity_and_host_target_approval() {
    for (caller, allow, expected_calls, success) in [
        (Some("alice.dhttp.net"), true, 2, true),
        (None, true, 0, false),
        (Some("bob.dhttp.net"), true, 0, false),
        (Some("alice.dhttp.net"), false, 1, false),
    ] {
        let sandbox = Sandbox::new("alice.dhttp.net", Default::default()).unwrap();
        let host = Arc::new(MockHost {
            calls: Mutex::new(Vec::new()),
            allow,
        });
        common::load_lib(
            &sandbox,
            "test",
            include_bytes!("fixtures/wasi-http-outgoing-client.wasm"),
        )
        .unwrap();
        let mut request = exchange::empty_request("/api/test/absent/small/normal?probe=1").await;
        request
            .headers_mut()
            .insert("pishoo-client-identity", "alice.dhttp.net".parse().unwrap());
        if let Some(caller) = caller {
            request
                .extensions_mut()
                .insert(OutgoingContext::new(caller, host.clone()));
        }
        let mut exchange = exchange::start(
            &sandbox,
            request,
            Limits {
                fuel: 100_000_000,
                memory_bytes: 64 << 20,
                deadline: Duration::from_secs(5),
            },
        )
        .await;
        let result = tokio::time::timeout(Duration::from_secs(2), exchange.result())
            .await
            .unwrap();
        if success {
            result.unwrap();
            let mut response = exchange.response().await.unwrap();
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
            let mut body = Vec::new();
            response.read_to_end(&mut body).await.unwrap();
            assert!(body.is_empty());
        } else {
            assert!(result.is_err());
        }
        let calls = host.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), expected_calls);
        assert!(calls.iter().all(|source| source == "alice.dhttp.net"));
        tokio::time::timeout(Duration::from_secs(2), async {
            while sandbox.usage() != Usage::default() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
}
