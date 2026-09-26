//! WASI HTTP preview 2 bridge for a single component version.
//!
//! Callers authorize and normalize the request before invoking this module. The
//! adapter streams body frames; it never collects the request or response.

use std::{
    fmt,
    sync::{Arc, Mutex},
    task::Poll,
    time::Duration,
};

use h3x::WriteResponse;
use http::Request;
use http_body::Frame;
use http_body_util::{BodyExt, StreamBody};
use qrecovery::send::CancelStream;
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    sync::oneshot,
};
use tokio_util::sync::CancellationToken;
use wasmtime::{
    Config, Engine, ResourceLimiter, Store, StoreLimits, StoreLimitsBuilder,
    component::{Component, Linker, ResourceTable},
};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView, filesystem::WasiFilesystemCtx};
use wasmtime_wasi_http::{
    WasiHttpCtx,
    p2::{
        WasiHttpCtxView, WasiHttpView,
        bindings::{Proxy, http::types},
    },
};

use crate::{
    outgoing::{AuthorizedOutgoing, HostOutgoing},
    sandbox::Reservation,
};

/// Per-invocation limits. Callers must choose these before a request starts.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub fuel: u64,
    pub memory_bytes: usize,
    pub deadline: Duration,
}

impl Limits {
    pub(crate) fn validate(self) -> Result<Self, Error> {
        if self.fuel == 0 || self.memory_bytes == 0 || self.deadline.is_zero() {
            return Err(Error::InvalidLimits);
        }
        Ok(self)
    }
}

#[derive(Debug)]
pub enum Error {
    InvalidLimits,
    SandboxCapacity,
    RouteNotFound,
    MethodNotAllowed,
    InvalidLibId,
    Component(wasmtime::Error),
    Execution(wasmtime::Error),
    GuestExitedWithoutResponse,
    GuestRejectedResponse(types::ErrorCode),
    Cancelled,
    Deadline,
    Transport(std::io::Error),
    InvalidScheme,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RouteNotFound => f.write_str("route not found in this sandbox"),
            Self::MethodNotAllowed => f.write_str("method not declared for this path"),
            Self::InvalidLibId => f.write_str("invalid library id"),
            Self::SandboxCapacity => f.write_str("identity sandbox capacity exceeded"),
            Self::InvalidLimits => f.write_str("invalid WASM execution limits"),
            Self::Component(_) => f.write_str("invalid WASM HTTP component"),
            Self::Execution(_) => f.write_str("WASM HTTP execution failed"),
            Self::GuestExitedWithoutResponse => f.write_str("guest exited without a response"),
            Self::GuestRejectedResponse(_) => f.write_str("guest rejected its response"),
            Self::Cancelled => f.write_str("WASM HTTP execution cancelled"),
            Self::Deadline => f.write_str("WASM HTTP execution deadline exceeded"),
            Self::Transport(error) => write!(f, "HTTP stream failed: {error}"),
            Self::InvalidScheme => f.write_str("request must have a host-normalized HTTP scheme"),
        }
    }
}

impl std::error::Error for Error {}

struct StoreData {
    table: ResourceTable,
    wasi: WasiCtx,
    http: WasiHttpCtx,
    limits: MemoryLimits,
    outgoing: HostOutgoing,
}

impl StoreData {
    fn new(memory_bytes: usize, filesystem: &WasiFilesystemCtx, outgoing: HostOutgoing) -> Self {
        let mut wasi = WasiCtx::builder().build();
        *wasi.filesystem() = filesystem.clone();
        Self {
            table: ResourceTable::new(),
            wasi,
            http: WasiHttpCtx::new(),
            limits: MemoryLimits {
                used: 0,
                pending: 0,
                ceiling: memory_bytes,
                base: StoreLimitsBuilder::new()
                    .memory_size(memory_bytes)
                    .instances(32)
                    .memories(32)
                    .tables(64)
                    .build(),
            },
            outgoing,
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
    config
        .wasm_component_model(true)
        .consume_fuel(true)
        .wasm_threads(false);
    let engine = Engine::new(&config).map_err(Error::Component)?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi_http::p2::add_to_linker_async(&mut linker).map_err(Error::Component)?;
    let runtime = Arc::new(Runtime { engine, linker });
    *slot = Some(runtime.clone());
    Ok(runtime)
}

/// A compiled library and its complete declared API document.
/// Execution capabilities belong to the sandbox and invocation, not this object.
pub struct WasmLib {
    pub id: String,
    pub openapi: oas3::OpenApiV3Spec,
    component: Component,
}

impl WasmLib {
    pub fn new(id: impl Into<String>, bytes: &[u8]) -> Result<Self, Error> {
        let id = id.into();
        if !crate::setup::valid_lib_id(&id) {
            return Err(Error::InvalidLibId);
        }
        let openapi = crate::setup::validate_lib(bytes)
            .map_err(|error| Error::Component(wasmtime::Error::msg(error.to_string())))?;
        let runtime = shared_runtime()?;
        let component = Component::from_binary(&runtime.engine, bytes).map_err(Error::Component)?;
        runtime
            .linker
            .instantiate_pre(&component)
            .map_err(Error::Component)?;
        Ok(Self {
            id,
            openapi,
            component,
        })
    }
}

/// One request, consuming host-authorized capabilities when it starts.
pub struct Invocation {
    lib: Arc<WasmLib>,
    local: Arc<str>,
    remote: Arc<str>,
    filesystem: WasiFilesystemCtx,
    outgoing: AuthorizedOutgoing,
    limits: Limits,
    qpack: h3x::ArcQpack,
}

impl Invocation {
    /// Prepare one HTTP/3 invocation with the accepted connection's QPACK context.
    pub fn new(
        lib: Arc<WasmLib>,
        local: Arc<str>,
        remote: Arc<str>,
        filesystem: WasiFilesystemCtx,
        outgoing: AuthorizedOutgoing,
        limits: Limits,
        qpack: h3x::ArcQpack,
    ) -> Result<Self, Error> {
        let limits = limits.validate()?;
        Ok(Self {
            lib,
            local,
            remote,
            filesystem,
            outgoing,
            limits,
            qpack,
        })
    }

    pub fn local(&self) -> &str {
        &self.local
    }

    pub fn remote(&self) -> &str {
        &self.remote
    }

    /// Own the complete HTTP/3 exchange. Body frames remain an internal WASI
    /// detail; callers pass the parsed h3x request and its response write stream.
    pub async fn handle<W>(
        self,
        ws: h3x::H3WriteStream<W>,
        request: h3x::Request<h3x::R>,
    ) -> Result<(), Error>
    where
        W: AsyncWrite + CancelStream + h3x::TransportError + Unpin + Send + 'static,
    {
        self.handle_admitted(ws, request, None).await
    }

    pub(crate) async fn handle_admitted<W>(
        self,
        ws: h3x::H3WriteStream<W>,
        request: h3x::Request<h3x::R>,
        reservation: Option<Arc<Reservation>>,
    ) -> Result<(), Error>
    where
        W: AsyncWrite + CancelStream + h3x::TransportError + Unpin + Send + 'static,
    {
        let cancel = self.outgoing.cancel().clone();
        let Self {
            lib,
            local,
            remote: _,
            filesystem,
            outgoing: authorized,
            limits,
            qpack,
        } = self;
        let deadline = limits.deadline;
        let _reservation = reservation.clone();
        let (mut parts, incoming) = request.into_parts();
        let method = parts.method.clone();
        let trailers = parts
            .extensions
            .remove::<h3x::Trailers>()
            .unwrap_or_default();
        let outgoing = h3x::ArcWndBuf::new(64 * 1024);
        let mut guard = CancelOnDrop::new(cancel.clone());
        guard.incoming = Some(incoming.clone());
        guard.outgoing = Some(outgoing.clone());
        let scheme = match parts.uri.scheme_str() {
            Some("http") => types::Scheme::Http,
            Some("https") => types::Scheme::Https,
            _ => return Err(Error::InvalidScheme),
        };
        let mut ended = false;
        let frames = futures::stream::poll_fn(move |cx| {
            if ended {
                return Poll::Ready(None);
            }
            incoming
                .poll_read_chunk(cx, 8192)
                .map(|result| match result {
                    Ok(chunk) if chunk.is_empty() => {
                        ended = true;
                        let trailers = trailers.headers();
                        (!trailers.is_empty()).then(|| Ok(Frame::trailers(trailers)))
                    }
                    Ok(chunk) => Some(Ok(Frame::data(chunk))),
                    Err(error) => {
                        ended = true;
                        Some(Err(error))
                    }
                })
        });
        let work = async {
            let request = Request::from_parts(parts, StreamBody::new(frames));
            let host_outgoing = HostOutgoing {
                identity: local,
                authorized,
            };
            let runtime = shared_runtime()?;
            let (response_tx, mut response_rx) = oneshot::channel();
            let (done_tx, mut done_rx) = oneshot::channel();
            let body = request.map(|body| {
                body.map_err(|error| types::ErrorCode::InternalError(Some(error.to_string())))
            });

            let guest_reservation = reservation;
            let guest = tokio::spawn(async move {
                let _reservation = guest_reservation;
                let mut store = Store::new(
                    &runtime.engine,
                    StoreData::new(limits.memory_bytes, &filesystem, host_outgoing),
                );
                store.limiter(|state| &mut state.limits);
                store.set_fuel(limits.fuel)?;
                store.fuel_async_yield_interval(Some(10_000))?;
                let proxy =
                    Proxy::instantiate_async(&mut store, &lib.component, &runtime.linker).await?;
                let incoming = store.data_mut().http().new_incoming_request(scheme, body)?;
                let outparam = store.data_mut().http().new_response_outparam(response_tx)?;
                proxy
                    .wasi_http_incoming_handler()
                    .call_handle(&mut store, incoming, outparam)
                    .await
            });

            let supervise_cancel = cancel.clone();
            tokio::spawn(async move {
                let mut guest = guest;
                let outcome = tokio::select! {
                    result = &mut guest => result.map_err(|e| Error::Execution(e.into())).and_then(|r| r.map_err(Error::Execution)),
                    _ = supervise_cancel.cancelled() => {
                        guest.abort();
                        let _ = guest.await;
                        Err(Error::Cancelled)
                    },
                };
                let _ = done_tx.send(outcome);
            });

            let mut completion = None;
            let response = tokio::select! {
                biased;
                response = &mut response_rx => {
                    match response {
                        Ok(response) => {
                            completion = Some(done_rx);
                            response.map_err(Error::GuestRejectedResponse)?
                        },
                        Err(_) => {
                            done_rx.await.map_err(|_| Error::GuestExitedWithoutResponse)??;
                            return Err(Error::GuestExitedWithoutResponse);
                        }
                    }
                }
                result = &mut done_rx => {
                    // The guest may submit a response and exit in the same poll.
                    if let Ok(response) = response_rx.try_recv() {
                        result.map_err(|_| Error::GuestExitedWithoutResponse)??;
                        response.map_err(Error::GuestRejectedResponse)?
                    } else {
                        result.map_err(|_| Error::GuestExitedWithoutResponse)??;
                        return Err(Error::GuestExitedWithoutResponse);
                    }
                }
            };

            let (head, mut body) = response.into_parts();
            let no_body = method == http::Method::HEAD
                || head.status == http::StatusCode::NO_CONTENT
                || head.status == http::StatusCode::NOT_MODIFIED;
            let mut response = h3x::Response::<h3x::W>::from_parts(head, outgoing.clone());
            if no_body {
                ws.write_response(response, method, qpack)
                    .await
                    .map_err(|e| Error::Transport(e.into()))?;
                // Send the legal bodyless response before cancelling its producer.
                cancel.cancel();
                drop(body);
                drop(completion);
                return Ok(());
            }
            let writing = ws.write_response(response.clone(), method, qpack);
            let pumping = async {
                let result = async {
                    while let Some(frame) = body.frame().await {
                        match frame.map_err(Error::GuestRejectedResponse)?.into_data() {
                            Ok(data) => response
                                .body()
                                .write_bytes(data)
                                .await
                                .map_err(Error::Transport)?,
                            Err(frame) => {
                                if let Ok(trailers) = frame.into_trailers() {
                                    for (name, value) in &trailers {
                                        response.append_trailer(name.clone(), value.clone());
                                    }
                                }
                            }
                        }
                    }
                    if let Some(done) = completion {
                        done.await
                            .map_err(|_| Error::GuestExitedWithoutResponse)??;
                    }
                    response.shutdown().await.map_err(Error::Transport)
                }
                .await;
                if result.is_err() {
                    let mut buffer = outgoing.clone();
                    buffer.cancel(h3x::ErrorCode::RequestCancelled.as_u64());
                }
                result
            };
            tokio::try_join!(
                async { writing.await.map_err(|e| Error::Transport(e.into())) },
                pumping
            )?;
            Ok(())
        };
        tokio::pin!(work);
        // Drop cleanup before the pending write future so h3x can reset its stream.
        let mut guard = guard;
        let result = tokio::select! {
            biased;
            result = &mut work => result,
            _ = cancel.cancelled() => Err(Error::Cancelled),
            _ = tokio::time::sleep(deadline) => Err(Error::Deadline),
        };
        if result.is_ok() {
            guard.cancel.take();
            guard.outgoing.take();
        }
        drop(guard);
        result
    }
}

struct CancelOnDrop {
    cancel: Option<CancellationToken>,
    incoming: Option<h3x::ArcWndBuf>,
    outgoing: Option<h3x::ArcWndBuf>,
}
impl CancelOnDrop {
    fn new(cancel: CancellationToken) -> Self {
        Self {
            cancel: Some(cancel),
            incoming: None,
            outgoing: None,
        }
    }
}
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
        }
        for buffer in [&mut self.incoming, &mut self.outgoing]
            .into_iter()
            .flatten()
        {
            buffer.cancel(h3x::ErrorCode::RequestCancelled.as_u64());
        }
    }
}

struct MemoryLimits {
    base: StoreLimits,
    ceiling: usize,
    used: usize,
    pending: usize,
}
impl ResourceLimiter for MemoryLimits {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let delta = desired.saturating_sub(current);
        if delta > self.ceiling - self.used
            || !self.base.memory_growing(current, desired, maximum)?
        {
            return Ok(false);
        }
        self.used += delta;
        self.pending = delta;
        Ok(true)
    }
    fn memory_grow_failed(&mut self, error: wasmtime::Error) -> wasmtime::Result<()> {
        self.used -= self.pending;
        self.pending = 0;
        self.base.memory_grow_failed(error)
    }
    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        self.base.table_growing(current, desired, maximum)
    }
    fn instances(&self) -> usize {
        self.base.instances()
    }
    fn memories(&self) -> usize {
        self.base.memories()
    }
    fn tables(&self) -> usize {
        self.base.tables()
    }
}

#[cfg(test)]
mod sandbox_tests {
    use wasmtime::component::Resource;
    use wasmtime_wasi::{
        filesystem::WasiFilesystemView,
        p2::bindings::filesystem::{
            preopens::Host,
            types::{DescriptorFlags, HostDescriptor, OpenFlags, PathFlags},
        },
    };

    use super::*;

    #[test]
    fn memory_limit_counts_all_memories_and_rolls_back_failed_growth() {
        let mut limit = MemoryLimits {
            base: StoreLimitsBuilder::new().build(),
            ceiling: 2 * 65536,
            used: 0,
            pending: 0,
        };
        assert!(limit.memory_growing(0, 65536, None).unwrap());
        assert!(limit.memory_growing(0, 65536, None).unwrap());
        assert!(!limit.memory_growing(0, 65536, None).unwrap());
        limit
            .memory_grow_failed(wasmtime::Error::msg("allocation failed"))
            .unwrap();
        assert!(limit.memory_growing(0, 65536, None).unwrap());
    }

    #[tokio::test]
    async fn identity_policy_excludes_ssl_and_enforces_read_only() {
        let root = std::env::temp_dir().join(format!(
            "pishoo-policy-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let alice = root.join("alice");
        std::fs::create_dir_all(alice.join("ssl")).unwrap();
        std::fs::create_dir_all(alice.join("db")).unwrap();
        std::fs::write(alice.join("ssl/key"), "secret").unwrap();
        std::fs::write(alice.join("db/value"), "public").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(alice.join("ssl"), alice.join("alias")).unwrap();
            std::os::unix::fs::symlink(alice.join("ssl/key"), alice.join("db/key")).unwrap();
        }
        let profile = dhttp_home::identity::IdentityProfile::try_from(alice).unwrap();
        let filesystem = crate::sandbox::read_identity_filesystem(&profile).unwrap();
        let mut state = StoreData::new(
            65536,
            &filesystem,
            HostOutgoing {
                identity: "alice".into(),
                authorized: AuthorizedOutgoing::denied(CancellationToken::new()),
            },
        );
        let mut view = state.filesystem();
        let dirs = Host::get_directories(&mut view).unwrap();
        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0].1, "/db");
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
        assert_eq!(view.read(file, 100, 0).await.unwrap().0, b"public");
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
        for path in ["../ssl/key", "/ssl/key", "key"] {
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
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn data_capabilities_are_private_and_survive_path_replacement() {
        let root = std::env::temp_dir().join(format!(
            "pishoo-data-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for name in ["first", "second"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
            std::fs::write(root.join(name).join("value"), name).unwrap();
        }
        let grant = |name| {
            let mut builder = WasiCtx::builder();
            builder
                .preopened_dir(
                    root.join(name),
                    "/data",
                    wasmtime_wasi::DirPerms::all(),
                    wasmtime_wasi::FilePerms::all(),
                )
                .unwrap();
            builder.build().filesystem().clone()
        };
        let a = grant("first");
        let b = grant("second");
        std::fs::rename(root.join("first"), root.join("saved")).unwrap();
        std::fs::create_dir(root.join("first")).unwrap();
        std::fs::write(root.join("first/value"), "replacement").unwrap();
        for (filesystem, expected) in [(a, "first"), (b, "second")] {
            let mut state = StoreData::new(
                65536,
                &filesystem,
                HostOutgoing {
                    identity: "alice".into(),
                    authorized: AuthorizedOutgoing::denied(CancellationToken::new()),
                },
            );
            let mut view = state.filesystem();
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
            assert_eq!(
                view.read(file, 100, 0).await.unwrap().0,
                expected.as_bytes()
            );
            for path in ["../second/value", "../ssl/privkey.pem", "/second/value"] {
                assert!(
                    view.open_at(
                        Resource::new_borrow(descriptor),
                        PathFlags::empty(),
                        path.into(),
                        OpenFlags::empty(),
                        DescriptorFlags::READ
                    )
                    .await
                    .is_err()
                );
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
