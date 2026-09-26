mod common;
#[path = "common/exchange.rs"]
mod exchange;
use std::time::Duration;

use exchange::{empty_request, start};
use pishoo::{
    sandbox::{Sandbox, SandboxLimits, Usage},
    wasm::{Error, Limits},
};
use tokio::io::AsyncReadExt;

const STREAM: &[u8] = include_bytes!("fixtures/wasi-http-stream-response-until-cancelled.wasm");
fn limits() -> Limits {
    Limits {
        fuel: 100_000_000,
        memory_bytes: 64 << 20,
        deadline: Duration::from_secs(5),
    }
}
fn sandbox(name: &str, requests: usize, memory: usize, fuel: u64) -> Sandbox {
    Sandbox::new(
        name,
        SandboxLimits {
            concurrent_requests: requests,
            memory_bytes: memory,
            in_flight_fuel: fuel,
            request_deadline: Duration::from_secs(5),
        },
    )
    .unwrap()
}
async fn reclaimed(s: &Sandbox) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while s.usage() != Usage::default() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
async fn rejected(s: &Sandbox, path: &str, limits: Limits) -> Error {
    let mut exchange = start(s, empty_request(path).await, limits).await;
    tokio::time::timeout(Duration::from_secs(2), exchange.result())
        .await
        .unwrap()
        .unwrap_err()
}
async fn running(s: &Sandbox, path: &str) -> (exchange::Exchange, h3x::Response<h3x::R>) {
    let mut exchange = start(s, empty_request(path).await, limits()).await;
    let response = tokio::time::timeout(Duration::from_secs(2), exchange.response())
        .await
        .unwrap()
        .unwrap();
    (exchange, response)
}

#[tokio::test]
async fn libraries_share_each_budget_and_release_it_after_drop() {
    for (requests, memory, fuel) in [
        (1, 128 << 20, 200_000_000),
        (2, 64 << 20, 200_000_000),
        (2, 128 << 20, 100_000_000),
    ] {
        let s = sandbox("alice", requests, memory, fuel);
        common::load_lib(&s, "first", STREAM).unwrap();
        common::load_lib(&s, "second", STREAM).unwrap();
        let active = running(&s, "/api/first/cancel").await;
        assert!(matches!(
            rejected(&s, "/api/second/cancel", limits()).await,
            Error::SandboxCapacity
        ));
        drop(active);
        reclaimed(&s).await;
        drop(running(&s, "/api/second/cancel").await);
        reclaimed(&s).await;
    }
}

#[tokio::test]
async fn cancellation_stops_all_identity_guests_but_not_other_identities() {
    let alice = sandbox("alice", 2, 128 << 20, 200_000_000);
    let bob = sandbox("bob", 1, 64 << 20, 100_000_000);
    common::load_lib(&alice, "a", STREAM).unwrap();
    common::load_lib(&alice, "b", STREAM).unwrap();
    common::load_lib(&bob, "c", STREAM).unwrap();
    let (mut a, mut ra) = running(&alice, "/api/a/cancel").await;
    let (mut b, mut rb) = running(&alice, "/api/b/cancel").await;
    let (c, mut rc) = running(&bob, "/api/c/cancel").await;
    alice.cancel();
    assert!(matches!(a.result().await, Err(Error::Cancelled)));
    assert!(matches!(b.result().await, Err(Error::Cancelled)));
    assert!(ra.read_to_end(&mut Vec::new()).await.is_err());
    assert!(rb.read_to_end(&mut Vec::new()).await.is_err());
    reclaimed(&alice).await;
    assert_eq!(bob.usage().requests, 1);
    assert!(rc.read(&mut [0; 12]).await.unwrap() > 0);
    assert!(matches!(
        rejected(&alice, "/api/a/cancel", limits()).await,
        Error::Cancelled
    ));
    drop((a, b, c, ra, rb, rc));
    reclaimed(&bob).await;
}

#[tokio::test]
async fn identity_cancellation_interrupts_a_pending_upload() {
    let s = sandbox("alice", 1, 64 << 20, 100_000_000);
    common::load_lib(
        &s,
        "test",
        include_bytes!("fixtures/wasi-http-read-request-then-respond.wasm"),
    )
    .unwrap();
    let mut exchange = start(&s, exchange::request("/api/test/pending"), limits()).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while s.usage().requests == 0 {
            tokio::task::yield_now().await;
        }
        s.cancel();
        assert!(matches!(exchange.result().await, Err(Error::Cancelled)));
    })
    .await
    .unwrap();
    reclaimed(&s).await;
}

#[tokio::test]
async fn deadline_and_invalid_limits_do_not_leak_admission() {
    let s = sandbox("alice", 1, 64 << 20, 100_000_000);
    common::load_lib(
        &s,
        "test",
        include_bytes!("fixtures/wasi-http-read-request-then-respond.wasm"),
    )
    .unwrap();
    let mut invalid = limits();
    invalid.fuel = 0;
    assert!(matches!(
        rejected(&s, "/api/test/pending", invalid).await,
        Error::InvalidLimits
    ));
    invalid = limits();
    invalid.deadline = Duration::from_secs(6);
    assert!(matches!(
        rejected(&s, "/api/test/pending", invalid).await,
        Error::SandboxCapacity
    ));
    assert_eq!(s.usage(), Usage::default());
    let mut short = limits();
    short.deadline = Duration::from_millis(30);
    let mut exchange = start(&s, exchange::request("/api/test/pending"), short).await;
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), exchange.result())
            .await
            .unwrap(),
        Err(Error::Deadline)
    ));
    reclaimed(&s).await;
}

#[tokio::test]
async fn library_lookup_replacement_and_removal_belong_to_the_sandbox() {
    let alice = sandbox("alice", 2, 128 << 20, 200_000_000);
    let bob = sandbox("bob", 1, 64 << 20, 100_000_000);
    common::load_lib(&alice, "live", STREAM).unwrap();
    assert!(matches!(
        rejected(&bob, "/api/live/cancel", limits()).await,
        Error::RouteNotFound
    ));
    assert_eq!(bob.usage(), Usage::default());
    assert!(common::load_lib(&alice, "live", b"invalid component").is_err());
    assert_eq!(alice.libs().len(), 1);
    let (active, mut response) = running(&alice, "/api/live/cancel").await;
    assert!(alice.remove_lib("live"));
    assert!(matches!(
        rejected(&alice, "/api/live/cancel", limits()).await,
        Error::RouteNotFound
    ));
    assert!(response.read(&mut [0; 12]).await.unwrap() > 0);
    drop((active, response));
    reclaimed(&alice).await;
    alice.cancel();
    assert!(matches!(
        common::load_lib(&alice, "live", STREAM),
        Err(Error::Cancelled)
    ));
}

#[tokio::test]
async fn routing_matches_declared_method_and_path_and_replaces_old_routes() {
    let s = sandbox("alice", 1, 64 << 20, 100_000_000);
    s.load_lib("live", &common::component(STREAM, &[("POST", "/cancel")]))
        .unwrap();
    let mut request = empty_request("/api/live/cancel").await;
    *request.method_mut() = http::Method::GET;
    let mut exchange = start(&s, request, limits()).await;
    assert!(matches!(
        exchange.result().await,
        Err(Error::MethodNotAllowed)
    ));
    for path in [
        "/api/live/undeclared",
        "/api/live",
        "/api/lively/cancel",
        "/cancel",
    ] {
        assert!(matches!(
            rejected(&s, path, limits()).await,
            Error::RouteNotFound
        ));
    }
    assert_eq!(s.usage(), Usage::default());
    assert!(s.load_lib("live", STREAM).is_err());
    drop(running(&s, "/api/live/cancel").await);
    reclaimed(&s).await;
    s.load_lib(
        "live",
        &common::component(STREAM, &[("POST", "/replacement")]),
    )
    .unwrap();
    assert!(matches!(
        rejected(&s, "/api/live/cancel", limits()).await,
        Error::RouteNotFound
    ));
    drop(running(&s, "/api/live/replacement?x=1").await);
    reclaimed(&s).await;
}
