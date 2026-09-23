//! WASI HTTP preview 2 bridge for a single component version.
//!
//! Callers authorize and normalize the request before invoking this module. The
//! adapter streams body frames; it never collects the request or response.

use std::{
    fmt,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use bytes::Bytes;
use http::{Request, Response};
use http_body::{Body, Frame, SizeHint};
use http_body_util::BodyExt;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use wasmtime::{
    Config, Engine, Store, StoreLimits, StoreLimitsBuilder,
    component::{Component, Linker, ResourceTable},
};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::{
    WasiHttpCtx,
    p2::{
        HttpResult, WasiHttpCtxView, WasiHttpHooks, WasiHttpView,
        bindings::{Proxy, http::types},
        body::HyperOutgoingBody,
        types::{HostFutureIncomingResponse, OutgoingRequestConfig},
    },
};

/// Per-invocation limits. Callers must choose these before a request starts.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub fuel: u64,
    pub memory_bytes: usize,
    pub deadline: Duration,
}

impl Limits {
    fn validate(self) -> Result<Self, Error> {
        if self.fuel == 0 || self.memory_bytes == 0 || self.deadline.is_zero() {
            return Err(Error::InvalidLimits);
        }
        Ok(self)
    }
}

#[derive(Debug)]
pub enum Error {
    InvalidLimits,
    Component(wasmtime::Error),
    Execution(wasmtime::Error),
    GuestExitedWithoutResponse,
    GuestRejectedResponse(types::ErrorCode),
    Cancelled,
    Deadline,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimits => f.write_str("invalid WASM execution limits"),
            Self::Component(_) => f.write_str("invalid WASM HTTP component"),
            Self::Execution(_) => f.write_str("WASM HTTP execution failed"),
            Self::GuestExitedWithoutResponse => f.write_str("guest exited without a response"),
            Self::GuestRejectedResponse(_) => f.write_str("guest rejected its response"),
            Self::Cancelled => f.write_str("WASM HTTP execution cancelled"),
            Self::Deadline => f.write_str("WASM HTTP execution deadline exceeded"),
        }
    }
}

impl std::error::Error for Error {}

struct DenyOutgoing;

impl WasiHttpHooks for DenyOutgoing {
    fn send_request(
        &mut self,
        _request: http::Request<HyperOutgoingBody>,
        _config: OutgoingRequestConfig,
    ) -> HttpResult<HostFutureIncomingResponse> {
        Err(types::ErrorCode::DestinationNotFound.into())
    }
}

struct StoreData {
    table: ResourceTable,
    wasi: WasiCtx,
    http: WasiHttpCtx,
    limits: StoreLimits,
    outgoing: DenyOutgoing,
}

impl StoreData {
    fn new(memory_bytes: usize) -> Self {
        Self {
            table: ResourceTable::new(),
            wasi: WasiCtx::builder().build(),
            http: WasiHttpCtx::new(),
            limits: StoreLimitsBuilder::new()
                .memory_size(memory_bytes)
                .instances(32)
                .memories(32)
                .tables(64)
                .build(),
            outgoing: DenyOutgoing,
        }
    }
}

impl WasiView for StoreData {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for StoreData {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: &mut self.outgoing,
        }
    }
}

struct Runtime {
    engine: Engine,
    linker: Linker<StoreData>,
}

static RUNTIME: Mutex<Option<Arc<Runtime>>> = Mutex::new(None);

fn shared_runtime() -> Result<Arc<Runtime>, Error> {
    let mut slot = RUNTIME
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(runtime) = &*slot {
        return Ok(runtime.clone());
    }
    let mut config = Config::new();
    config.wasm_component_model(true).consume_fuel(true);
    let engine = Engine::new(&config).map_err(Error::Component)?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi_http::p2::add_to_linker_async(&mut linker).map_err(Error::Component)?;
    let runtime = Arc::new(Runtime { engine, linker });
    *slot = Some(runtime.clone());
    Ok(runtime)
}

struct Inner {
    runtime: Arc<Runtime>,
    component: Component,
    active: AtomicUsize,
}

struct ActiveExecution(Arc<Inner>);

impl Drop for ActiveExecution {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::Release);
    }
}

/// A compiled, immutable WASI HTTP component. No guest Store is reused.
#[derive(Clone)]
pub struct WasmHttpApp(Arc<Inner>);

impl WasmHttpApp {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let runtime = shared_runtime()?;
        let component = Component::from_binary(&runtime.engine, bytes).map_err(Error::Component)?;
        // Reject missing or unsupported imports before this version is published.
        runtime
            .linker
            .instantiate_pre(&component)
            .map_err(Error::Component)?;
        Ok(Self(Arc::new(Inner {
            runtime,
            component,
            active: AtomicUsize::new(0),
        })))
    }

    pub fn active_guest_tasks(&self) -> usize {
        self.0.active.load(Ordering::Acquire)
    }

    /// The scheme comes from trusted transport metadata, never an HTTP header.
    pub async fn invoke<B>(
        &self,
        request: Request<B>,
        scheme: types::Scheme,
        limits: Limits,
    ) -> Result<Response<TrackedBody>, Error>
    where
        B: Body<Data = Bytes> + Send + 'static,
        B::Error: fmt::Display + Send + Sync + 'static,
    {
        let limits = limits.validate()?;
        let cancel = CancellationToken::new();
        let mut pending = CancelOnDrop(Some(cancel.clone()));
        let app = self.0.clone();
        let (response_tx, mut response_rx) = oneshot::channel();
        let (done_tx, mut done_rx) = oneshot::channel();
        let body = request.map(|body| {
            body.map_err(|error| types::ErrorCode::InternalError(Some(error.to_string())))
        });

        app.active.fetch_add(1, Ordering::Release);
        let execution = ActiveExecution(app.clone());
        let guest = tokio::spawn(async move {
            let mut store = Store::new(&app.runtime.engine, StoreData::new(limits.memory_bytes));
            store.limiter(|state| &mut state.limits);
            store.set_fuel(limits.fuel)?;
            let proxy =
                Proxy::instantiate_async(&mut store, &app.component, &app.runtime.linker).await?;
            let incoming = store.data_mut().http().new_incoming_request(scheme, body)?;
            let outparam = store.data_mut().http().new_response_outparam(response_tx)?;
            proxy
                .wasi_http_incoming_handler()
                .call_handle(&mut store, incoming, outparam)
                .await
        });

        let supervise_cancel = cancel.clone();
        tokio::spawn(async move {
            let _execution = execution;
            let mut guest = guest;
            let outcome = tokio::select! {
                result = &mut guest => result.map_err(|e| Error::Execution(e.into())).and_then(|r| r.map_err(Error::Execution)),
                _ = supervise_cancel.cancelled() => {
                    guest.abort();
                    let _ = guest.await;
                    Err(Error::Cancelled)
                },
                _ = tokio::time::sleep(limits.deadline) => {
                    supervise_cancel.cancel();
                    guest.abort();
                    let _ = guest.await;
                    Err(Error::Deadline)
                },
            };
            let _ = done_tx.send(outcome);
        });

        let response = tokio::select! {
            biased;
            response = &mut response_rx => {
                match response {
                    Ok(response) => response.map_err(Error::GuestRejectedResponse)?,
                    Err(_) => {
                        done_rx.await.map_err(|_| Error::GuestExitedWithoutResponse)??;
                        return Err(Error::GuestExitedWithoutResponse);
                    }
                }
            }
            result = &mut done_rx => {
                // The guest may submit a response and exit in the same poll.
                if let Ok(response) = response_rx.try_recv() {
                    response.map_err(Error::GuestRejectedResponse)?
                } else {
                    result.map_err(|_| Error::GuestExitedWithoutResponse)??;
                    return Err(Error::GuestExitedWithoutResponse);
                }
            }
        };
        pending.0.take();
        let (parts, body) = response.into_parts();
        Ok(Response::from_parts(
            parts,
            TrackedBody {
                body,
                cancel,
                ended: false,
            },
        ))
    }
}

struct CancelOnDrop(Option<CancellationToken>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(cancel) = &self.0 {
            cancel.cancel();
        }
    }
}

/// Cancels guest execution if the original response body is discarded early.
pub struct TrackedBody {
    body: HyperOutgoingBody,
    cancel: CancellationToken,
    ended: bool,
}

impl Body for TrackedBody {
    type Data = Bytes;
    type Error = types::ErrorCode;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        match Pin::new(&mut self.body).poll_frame(cx) {
            Poll::Ready(None) => {
                self.ended = true;
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(error))) => {
                self.cancel.cancel();
                Poll::Ready(Some(Err(error)))
            }
            other => other,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

impl Drop for TrackedBody {
    fn drop(&mut self) {
        if !self.ended {
            self.cancel.cancel();
        }
    }
}
