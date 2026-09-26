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

mod identity {
    wasmtime::component::bindgen!({
        path: "../wit/pishoo-identity",
        inline: "package pishoo:host; world identity-host { import pishoo:identity/signatures@0.1.0; }",
        imports: { default: async },
    });
}

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

impl Default for LibPolicy {
    fn default() -> Self {
        Self {
            data_write: true,
            outgoing: Vec::new(),
            sign: false,
            verify: false,
        }
    }
}

impl Runtime {
    pub(crate) fn new() -> Result<Self> {
        let mut config = Config::new();
        config
            .wasm_component_model(true)
            .consume_fuel(true)
            .wasm_threads(false);
        let engine = Engine::new(&config).map_err(Error::Guest)?;
        let mut linker = Linker::new(&engine);
        wasmtime_wasi_http::p2::add_to_linker_async(&mut linker).map_err(Error::Guest)?;
        identity::IdentityHost::add_to_linker::<_, wasmtime::component::HasSelf<_>>(
            &mut linker,
            |state| state,
        )
        .map_err(Error::Guest)?;
        Ok(Self { engine, linker })
    }

    pub(crate) fn compile(&self, bytes: &[u8]) -> Result<Component> {
        let component = Component::from_binary(&self.engine, bytes)
            .map_err(|error| Error::InvalidComponent(error.to_string()))?;
        let pre = self
            .linker
            .instantiate_pre(&component)
            .map_err(|error| Error::InvalidComponent(error.to_string()))?;
        ProxyPre::new(pre).map_err(|error| Error::InvalidComponent(error.to_string()))?;
        Ok(component)
    }
}

impl Lib {
    pub(crate) fn load(
        runtime: Arc<Runtime>,
        id: String,
        bytes: &[u8],
        data_dir: &Path,
        policy: LibPolicy,
        cancel: CancellationToken,
    ) -> Result<Self> {
        if id.len() > 63
            || !id.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(Error::InvalidComponent("invalid Lib id".into()));
        }
        let openapi = validate_lib(bytes)?;
        let digest = Sha256::digest(bytes).into();
        let component = runtime.compile(bytes)?;
        match std::fs::symlink_metadata(data_dir) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(Error::InvalidComponent(
                    "Lib data must be a real directory".into(),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(data_dir).map_err(Error::Io)?;
            }
            Err(error) => return Err(Error::Io(error)),
        }
        let parent = data_dir
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let parent = std::fs::canonicalize(parent).map_err(Error::Io)?;
        let data = std::fs::canonicalize(data_dir).map_err(Error::Io)?;
        if data.parent() != Some(parent.as_path())
            || std::fs::symlink_metadata(data_dir)
                .map_err(Error::Io)?
                .file_type()
                .is_symlink()
        {
            return Err(Error::InvalidComponent(
                "Lib data resolves outside its directory".into(),
            ));
        }
        let mut builder = WasiCtx::builder();
        let (directories, files) = if policy.data_write {
            (
                wasmtime_wasi::DirPerms::all(),
                wasmtime_wasi::FilePerms::all(),
            )
        } else {
            (
                wasmtime_wasi::DirPerms::READ,
                wasmtime_wasi::FilePerms::READ,
            )
        };
        builder
            .preopened_dir(data, "/data", directories, files)
            .map_err(Error::Guest)?;
        let filesystem = builder.build().filesystem().clone();
        Ok(Self {
            id,
            digest,
            openapi,
            component,
            runtime,
            filesystem,
            policy,
            cancel,
        })
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

impl Invocation {
    pub(crate) fn new(
        lib: Arc<Lib>,
        permit: tokio::sync::OwnedSemaphorePermit,
        endpoint: dhttp::Endpoint,
        handshake: &dhttp::HandshakeSummary,
        tasks: TaskTracker,
    ) -> Result<Self> {
        let local = handshake.local.as_ref().ok_or(Error::MissingHandshake)?;
        if local.name() != endpoint.name() {
            return Err(Error::IdentityMismatch);
        }
        if lib.cancel.is_cancelled() || tasks.is_closed() {
            return Err(Error::Cancelled);
        }
        let producer_cancel = lib.cancel.child_token();
        Ok(Self {
            lib,
            local: local.clone(),
            remote: handshake.remote.clone(),
            endpoint,
            producer_cancel,
            permit,
            tasks,
        })
    }

    pub(crate) async fn execute(self, request: Request<Body>) -> Result<Response<Body>> {
        let Self {
            lib,
            local,
            remote,
            endpoint,
            producer_cancel,
            permit,
            tasks,
        } = self;
        let cancel_on_drop = producer_cancel.clone().drop_guard();
        if producer_cancel.is_cancelled() || tasks.is_closed() {
            return Err(Error::Cancelled);
        }
        let scheme = match request.uri().scheme_str() {
            Some("http") => types::Scheme::Http,
            Some("https") => types::Scheme::Https,
            _ => {
                return Err(Error::BadRequest(
                    "Lib request requires an HTTP scheme".into(),
                ));
            }
        };
        let children = TaskTracker::new();
        let outgoing_cancel = producer_cancel.child_token();
        let outgoing = HostOutgoing {
            endpoint: remote
                .as_ref()
                .filter(|caller| caller.name() == local.name())
                .map(|_| endpoint),
            policy: copy_policy(&lib.policy),
            remaining_requests: 16,
            children: children.clone(),
            cancel: outgoing_cancel.clone(),
        };
        let mut wasi = WasiCtx::builder().build();
        *wasi.filesystem() = lib.filesystem.clone();
        let store_data = StoreData {
            table: ResourceTable::new(),
            wasi,
            http: WasiHttpCtx::new(),
            memory: MemoryLimits {
                base: StoreLimitsBuilder::new()
                    .memory_size(64 << 20)
                    .instances(32)
                    .memories(32)
                    .tables(64)
                    .table_elements(100_000)
                    .build(),
                used: 0,
                pending: 0,
            },
            outgoing,
            local,
            remote,
            policy: copy_policy(&lib.policy),
            permit,
        };
        let request = request.map(|body| {
            body.map_err(|error| types::ErrorCode::InternalError(Some(error.to_string())))
        });
        let (response_tx, mut response_rx) = oneshot::channel();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        // The supervisor owns and reaps the actual guest task even if nobody
        // polls the HTTP response body again after receiving its headers.
        let mut supervisor = tasks.spawn(async move {
            let mut guest = tokio::spawn(async move {
                let mut store = Store::new(&lib.runtime.engine, store_data);
                store.limiter(|state| &mut state.memory);
                store.set_fuel(100_000_000).map_err(Error::Guest)?;
                store
                    .fuel_async_yield_interval(Some(10_000))
                    .map_err(Error::Guest)?;
                let proxy =
                    Proxy::instantiate_async(&mut store, &lib.component, &lib.runtime.linker)
                        .await
                        .map_err(Error::Guest)?;
                let incoming = store
                    .data_mut()
                    .http()
                    .new_incoming_request(scheme, request)
                    .map_err(Error::Guest)?;
                let outparam = store
                    .data_mut()
                    .http()
                    .new_response_outparam(response_tx)
                    .map_err(Error::Guest)?;
                proxy
                    .wasi_http_incoming_handler()
                    .call_handle(&mut store, incoming, outparam)
                    .await
                    .map_err(Error::Guest)
            });
            let outcome = tokio::select! {
                biased;
                _ = producer_cancel.cancelled() => {
                    guest.abort();
                    let _ = guest.await;
                    Err(Error::Cancelled)
                }
                _ = tokio::time::sleep_until(deadline) => {
                    producer_cancel.cancel();
                    guest.abort();
                    let _ = guest.await;
                    Err(Error::Deadline)
                }
                outcome = &mut guest => outcome.map_err(Error::Task).and_then(|outcome| outcome),
            };
            outgoing_cancel.cancel();
            children.close();
            children.wait().await;
            outcome
        });

        let (response, guest) = tokio::select! {
            biased;
            // Read the result, rather than is_finished(), so an error cannot
            // race a successful response-head submission unnoticed.
            outcome = &mut supervisor => {
                outcome.map_err(Error::Task)??;
                let response = response_rx.try_recv().map_err(|_| Error::GuestExitedWithoutResponse)?
                    .map_err(Error::GuestRejectedResponse)?;
                (response, None)
            }
            response = &mut response_rx => {
                let response = match response {
                    Ok(response) => response.map_err(Error::GuestRejectedResponse)?,
                    Err(_) => {
                        supervisor.await.map_err(Error::Task)??;
                        return Err(Error::GuestExitedWithoutResponse);
                    }
                };
                // If both became ready during this poll, consume the task
                // result now. Pending leaves its wake registration intact.
                match futures::poll!(&mut supervisor) {
                    Poll::Ready(outcome) => { outcome.map_err(Error::Task)??; (response, None) }
                    Poll::Pending => (response, Some(supervisor)),
                }
            }
        };
        let (parts, inner) = response.into_parts();
        Ok(Response::from_parts(
            parts,
            LibResponseBody::Reading {
                inner,
                guest,
                cancel_on_drop,
            }
            .map_err(Error::body_error)
            .boxed_unsync(),
        ))
    }
}

impl HttpBody for LibResponseBody {
    type Data = Bytes;
    type Error = Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>>>> {
        let this = self.get_mut();
        loop {
            match this {
                Self::Ended => return Poll::Ready(None),
                Self::Reading { inner, guest, .. } => {
                    if let Some(task) = guest {
                        if let Poll::Ready(outcome) = Pin::new(task).poll(cx) {
                            *guest = None;
                            if let Err(error) =
                                outcome.map_err(Error::Task).and_then(|outcome| outcome)
                            {
                                *this = Self::Ended;
                                return Poll::Ready(Some(Err(error)));
                            }
                        }
                    }
                    let trailers = match Pin::new(inner).poll_frame(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Some(Ok(frame))) if frame.is_data() => {
                            return Poll::Ready(Some(Ok(frame)));
                        }
                        Poll::Ready(Some(Ok(frame))) => frame.into_trailers().ok(),
                        Poll::Ready(Some(Err(error))) => {
                            *this = Self::Ended;
                            return Poll::Ready(Some(Err(Error::GuestRejectedResponse(error))));
                        }
                        Poll::Ready(None) => None,
                    };
                    let Self::Reading {
                        guest,
                        cancel_on_drop,
                        ..
                    } = std::mem::replace(this, Self::Ended)
                    else {
                        unreachable!()
                    };
                    match guest {
                        Some(guest) => {
                            *this = Self::Waiting {
                                guest,
                                trailers,
                                cancel_on_drop,
                            }
                        }
                        None => {
                            cancel_on_drop.disarm();
                            return Poll::Ready(
                                trailers.map(|headers| Ok(Frame::trailers(headers))),
                            );
                        }
                    }
                }
                Self::Waiting { guest, .. } => {
                    let outcome = match Pin::new(guest).poll(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(outcome) => {
                            outcome.map_err(Error::Task).and_then(|outcome| outcome)
                        }
                    };
                    let Self::Waiting {
                        trailers,
                        cancel_on_drop,
                        ..
                    } = std::mem::replace(this, Self::Ended)
                    else {
                        unreachable!()
                    };
                    match outcome {
                        Ok(()) => {
                            cancel_on_drop.disarm();
                            return Poll::Ready(
                                trailers.map(|headers| Ok(Frame::trailers(headers))),
                            );
                        }
                        Err(error) => return Poll::Ready(Some(Err(error))),
                    }
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        matches!(self, Self::Ended)
    }
}

impl ResourceLimiter for MemoryLimits {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        self.pending = 0;
        let delta = desired.saturating_sub(current);
        if delta > (64usize << 20).saturating_sub(self.used)
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

impl WasiHttpHooks for HostOutgoing {
    fn send_request(
        &mut self,
        mut request: Request<HyperOutgoingBody>,
        config: OutgoingRequestConfig,
    ) -> HttpResult<HostFutureIncomingResponse> {
        let endpoint = self
            .endpoint
            .as_ref()
            .ok_or(types::ErrorCode::HttpRequestDenied)?
            .clone();
        if self.cancel.is_cancelled()
            || self.remaining_requests == 0
            || !outgoing_allowed(&self.policy, request.method(), request.uri())
        {
            return Err(types::ErrorCode::HttpRequestDenied.into());
        }
        self.remaining_requests -= 1;
        let reserved: Vec<_> = request
            .headers()
            .keys()
            .filter(|name| name.as_str().starts_with("pishoo-"))
            .cloned()
            .collect();
        for name in reserved {
            request.headers_mut().remove(name);
        }
        let cancel = self.cancel.clone();
        let children = self.children.clone();
        let pending = self.children.spawn(async move {
            let response = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Ok(Err(types::ErrorCode::HttpRequestDenied)),
                response = tokio::time::timeout(config.connect_timeout.saturating_add(config.first_byte_timeout), endpoint.from_request(request)) => {
                    match response {
                        Ok(Ok(response)) => response,
                        Ok(Err(error)) => return Ok(Err(types::ErrorCode::InternalError(Some(error.to_string())))),
                        Err(_) => return Ok(Err(types::ErrorCode::ConnectionTimeout)),
                    }
                }
            };
            let (parts, mut body) = response.into_parts();
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            let worker = children.spawn(async move {
                let pumping = async {
                    while let Some(frame) = body.frame().await {
                        match frame {
                            Ok(frame) => match frame.into_data() {
                                Ok(mut bytes) => {
                                    while !bytes.is_empty() {
                                        let chunk = bytes.split_to(bytes.len().min(16 * 1024));
                                        if tx.send(Ok(Frame::data(chunk))).await.is_err() { return; }
                                    }
                                }
                                Err(frame) => { if tx.send(Ok(frame)).await.is_err() { return; } }
                            },
                            Err(error) => {
                                let _ = tx.send(Err(types::ErrorCode::InternalError(Some(error.to_string())))).await;
                                return;
                            }
                        }
                    }
                };
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {},
                    _ = pumping => {},
                }
            });
            let frames = futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|frame| (frame, rx)) });
            Ok(Ok(IncomingResponse {
                resp: Response::from_parts(parts, StreamBody::new(Box::pin(frames)).boxed_unsync()),
                worker: Some(worker.into()),
                between_bytes_timeout: config.between_bytes_timeout,
            }))
        });
        Ok(HostFutureIncomingResponse::pending(pending.into()))
    }

    fn outgoing_body_buffer_chunks(&mut self) -> usize {
        1
    }
    fn outgoing_body_chunk_size(&mut self) -> usize {
        16 * 1024
    }
}

impl identity::pishoo::identity::signatures::Host for StoreData {
    async fn sign(
        &mut self,
        data: Vec<u8>,
    ) -> std::result::Result<Vec<u8>, identity::pishoo::identity::signatures::SignError> {
        use identity::pishoo::identity::signatures::SignError;
        if !self.policy.sign {
            return Err(SignError::Denied);
        }
        if data.len() > 1024 * 1024 {
            return Err(SignError::InputTooLarge);
        }
        if self.outgoing.cancel.is_cancelled() {
            return Err(SignError::Unavailable);
        }
        let signature =
            dhttp::certificate::sign(&self.local, &data).map_err(|_| SignError::Failed)?;
        if signature.len() > 8192 {
            return Err(SignError::Failed);
        }
        Ok(signature)
    }

    async fn verify(
        &mut self,
        signature: Vec<u8>,
        data: Vec<u8>,
        name: String,
    ) -> std::result::Result<bool, identity::pishoo::identity::signatures::VerifyError> {
        use identity::pishoo::identity::signatures::VerifyError;
        if !self.policy.verify {
            return Err(VerifyError::Unavailable);
        }
        if data.len() > 1024 * 1024 || signature.len() > 8192 {
            return Err(VerifyError::InputTooLarge);
        }
        if self.outgoing.cancel.is_cancelled() {
            return Err(VerifyError::Unavailable);
        }
        let name = dhttp_home::normalize_name(&name).ok_or(VerifyError::InvalidIdentity)?;
        if name == self.local.name() {
            return dhttp::certificate::verify_signature(
                self.local.public_key().as_ref(),
                &data,
                &signature,
            )
            .map_err(|_| VerifyError::Failed);
        }
        if let Some(remote) = &self.remote {
            if name == remote.name() {
                return dhttp::certificate::verify_signature(
                    remote.public_key().as_ref(),
                    &data,
                    &signature,
                )
                .map_err(|_| VerifyError::Failed);
            }
        }
        let endpoint = self
            .outgoing
            .endpoint
            .as_ref()
            .ok_or(VerifyError::Unavailable)?;
        let uri = format!("https://{name}/")
            .parse()
            .map_err(|_| VerifyError::InvalidIdentity)?;
        if self.outgoing.remaining_requests == 0
            || !outgoing_allowed(&self.outgoing.policy, &Method::GET, &uri)
        {
            return Err(VerifyError::Unavailable);
        }
        self.outgoing.remaining_requests -= 1;
        let remote = tokio::select! {
            biased;
            _ = self.outgoing.cancel.cancelled() => return Err(VerifyError::Unavailable),
            remote = dhttp::certificate::resolve_remote(endpoint, &name) => remote.map_err(|_| VerifyError::UnknownIdentity)?,
        };
        dhttp::certificate::verify_signature(remote.public_key().as_ref(), &data, &signature)
            .map_err(|_| VerifyError::Failed)
    }
}

fn copy_policy(policy: &LibPolicy) -> LibPolicy {
    LibPolicy {
        data_write: policy.data_write,
        outgoing: policy
            .outgoing
            .iter()
            .map(|rule| OutgoingRule {
                methods: rule.methods.clone(),
                origin: rule.origin.clone(),
                path_prefix: rule.path_prefix.clone(),
            })
            .collect(),
        sign: policy.sign,
        verify: policy.verify,
    }
}

fn outgoing_allowed(policy: &LibPolicy, method: &Method, uri: &Uri) -> bool {
    let Some(host) = uri.host().and_then(dhttp_home::normalize_name) else {
        return false;
    };
    if !matches!(uri.scheme_str(), Some("http" | "https"))
        || uri
            .authority()
            .is_none_or(|authority| authority.as_str().contains('@'))
    {
        return false;
    }
    // Reject ambiguous escaping and normalization before comparing a capability
    // prefix or reserved management route. Guest URLs cannot smuggle dot paths.
    let path = uri.path();
    let mut decoded = Vec::with_capacity(path.len());
    let mut bytes = path.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let Some(high) = bytes.next().and_then(|byte| (byte as char).to_digit(16)) else {
                return false;
            };
            let Some(low) = bytes.next().and_then(|byte| (byte as char).to_digit(16)) else {
                return false;
            };
            let byte = (high * 16 + low) as u8;
            if matches!(byte, b'/' | b'\\' | b'%' | 0) {
                return false;
            }
            decoded.push(byte);
        } else {
            decoded.push(byte);
        }
    }
    let Ok(path) = std::str::from_utf8(&decoded) else {
        return false;
    };
    if path.contains('\\') || path.split('/').any(|part| part == "." || part == "..") {
        return false;
    }
    if [
        "/acl",
        "/contact",
        "/contacts",
        "/workspace",
        "/workspace-api",
    ]
    .iter()
    .any(|prefix| {
        path == *prefix
            || path
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('/'))
    }) {
        return false;
    }
    policy.outgoing.iter().any(|rule| {
        rule.methods.contains(method)
            && rule.origin.scheme() == uri.scheme()
            && rule
                .origin
                .host()
                .and_then(dhttp_home::normalize_name)
                .as_ref()
                == Some(&host)
            && rule
                .origin
                .port_u16()
                .or_else(|| match rule.origin.scheme_str() {
                    Some("http") => Some(80),
                    Some("https") => Some(443),
                    _ => None,
                })
                == uri.port_u16().or_else(|| match uri.scheme_str() {
                    Some("http") => Some(80),
                    Some("https") => Some(443),
                    _ => None,
                })
            && rule.path_prefix.starts_with('/')
            && (path == rule.path_prefix
                || path
                    .strip_prefix(&rule.path_prefix)
                    .is_some_and(|rest| rule.path_prefix.ends_with('/') || rest.starts_with('/')))
    })
}

#[cfg(test)]
mod tests;
