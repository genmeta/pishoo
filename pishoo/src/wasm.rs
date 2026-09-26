//! Per-request WASI HTTP execution and host capabilities.
//!
//! Admission belongs to the Server's semaphore; the permit follows the actual
//! Store until it is dropped. Transport ownership stays in standard HTTP bodies.

use std::{
    future::Future,
    path::Path,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use bytes::Bytes;
use http::{Method, Request, Response, Uri};
use http_body::{Body as HttpBody, Frame};
use http_body_util::{BodyExt, StreamBody};
use sha2::{Digest, Sha256};
use tokio::{sync::oneshot, task::JoinHandle};
use tokio_util::{
    sync::{CancellationToken, DropGuard},
    task::TaskTracker,
};
use wasmtime::{
    Config, Engine, ResourceLimiter, Store, StoreLimits, StoreLimitsBuilder,
    component::{Component, Linker, ResourceTable},
};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView, filesystem::WasiFilesystemCtx};
use wasmtime_wasi_http::{
    WasiHttpCtx,
    p2::{
        HttpResult, WasiHttpCtxView, WasiHttpHooks, WasiHttpView,
        bindings::{Proxy, ProxyPre, http::types},
        body::HyperOutgoingBody,
        types::{HostFutureIncomingResponse, IncomingResponse, OutgoingRequestConfig},
    },
};

use crate::{Body, Error, Result, validate_lib};

pub(crate) struct Runtime {
    engine: Engine,
    linker: Linker<StoreData>,
}

pub(crate) struct Lib {
    pub(crate) id: String,
    pub(crate) digest: [u8; 32],
    pub(crate) openapi: oas3::OpenApiV3Spec,
    component: Component,
    runtime: Arc<Runtime>,
    filesystem: WasiFilesystemCtx,
    policy: LibPolicy,
    pub(crate) cancel: CancellationToken,
}

pub(crate) struct LibPolicy {
    pub(crate) data_write: bool,
    pub(crate) outgoing: Vec<OutgoingRule>,
    pub(crate) sign: bool,
    pub(crate) verify: bool,
}

pub(crate) struct OutgoingRule {
    pub(crate) methods: Vec<Method>,
    pub(crate) origin: Uri,
    pub(crate) path_prefix: String,
}

struct StoreData {
    table: ResourceTable,
    wasi: WasiCtx,
    http: WasiHttpCtx,
    memory: MemoryLimits,
    outgoing: HostOutgoing,
    local: dhttp::LocalAuthority,
    remote: Option<dhttp::RemoteAuthority>,
    policy: LibPolicy,
    // The independent admission resource is deliberately retained by the Store.
    #[allow(dead_code)]
    permit: tokio::sync::OwnedSemaphorePermit,
}

struct MemoryLimits {
    base: StoreLimits,
    used: usize,
    pending: usize,
}

pub(crate) struct Invocation {
    lib: Arc<Lib>,
    local: dhttp::LocalAuthority,
    remote: Option<dhttp::RemoteAuthority>,
    endpoint: dhttp::Endpoint,
    producer_cancel: CancellationToken,
    permit: tokio::sync::OwnedSemaphorePermit,
    tasks: TaskTracker,
}

enum LibResponseBody {
    Reading {
        inner: HyperOutgoingBody,
        guest: Option<JoinHandle<Result<()>>>,
        cancel_on_drop: DropGuard,
    },
    Waiting {
        guest: JoinHandle<Result<()>>,
        trailers: Option<http::HeaderMap>,
        cancel_on_drop: DropGuard,
    },
    Ended,
}

struct HostOutgoing {
    endpoint: Option<dhttp::Endpoint>,
    policy: LibPolicy,
    remaining_requests: usize,
    children: TaskTracker,
    cancel: CancellationToken,
}

// Keep these implementation files in the same execution module: private
// resources and helpers do not become a second cross-module interface.
include!("wasm/runtime.rs");
include!("wasm/invocation.rs");
include!("wasm/body.rs");
include!("wasm/outgoing.rs");
include!("wasm/identity.rs");

#[cfg(test)]
#[path = "../tests/unit/wasm/mod.rs"]
mod tests;
