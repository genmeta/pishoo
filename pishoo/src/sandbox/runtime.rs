//! Component loading, Store isolation, invocation execution, and response lifecycle.

use std::{
    future::Future,
    path::Path,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use bytes::Bytes;
use http::{Request, Response};
use http_body::{Body as HttpBody, Frame};
use http_body_util::BodyExt;
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use wasmtime::{
    Config, Engine, ResourceLimiter, Store, StoreLimitsBuilder,
    component::{Component, Linker, ResourceTable},
};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::{
    WasiHttpCtx,
    p2::{
        WasiHttpCtxView, WasiHttpView,
        bindings::{Proxy, ProxyPre, http::types},
    },
};

use super::{
    HostOutgoing, Invocation, Lib, LibPolicy, LibResponseBody, MemoryLimits, OutgoingRule,
    StoreData, WasmRuntime, host::identity, validate_lib,
};
use crate::{Body, Error, Result};

// Component compilation, per-version filesystem grants, and Store limits.

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

impl WasmRuntime {
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
        runtime: Arc<WasmRuntime>,
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

// One invocation owns its Store and supervises guest and outgoing work.

impl Invocation {
    pub(crate) fn new(
        lib: Arc<Lib>,
        endpoint: dhttp::Endpoint,
        handshake: &dhttp::HandshakeSummary,
        tasks: TaskTracker,
    ) -> Result<Self> {
        let local = handshake.local.as_ref().ok_or(Error::MissingHandshake)?;
        if local.name() != endpoint.name() {
            return Err(Error::IdentityMismatch);
        }
        Ok(Self {
            lib,
            local: local.clone(),
            remote: handshake.remote.clone(),
            endpoint,
            tasks,
        })
    }

    pub(crate) async fn execute(self, request: Request<Body>) -> Result<Response<Body>> {
        let Self {
            lib,
            local,
            remote,
            endpoint,
            tasks,
        } = self;
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
        let outgoing_cancel = lib.cancel.child_token();
        let lib_cancel = lib.cancel.clone();
        let outgoing = HostOutgoing {
            endpoint: remote
                .as_ref()
                .filter(|caller| caller.name() == local.name())
                .map(|_| endpoint),
            policy: copy_policy(&lib.policy),
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
        };
        let request = request.map(|body| {
            body.map_err(|error| types::ErrorCode::InternalError(Some(error.to_string())))
        });
        let (response_tx, mut response_rx) = oneshot::channel();
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
                _ = lib_cancel.cancelled() => {
                    guest.abort();
                    let _ = guest.await;
                    Err(Error::Cancelled)
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
            LibResponseBody::Reading { inner, guest }
                .map_err(Error::body_error)
                .boxed_unsync(),
        ))
    }
}

// Response frames retain the guest result until normal completion.

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
                    if let Some(task) = guest
                        && let Poll::Ready(outcome) = Pin::new(task).poll(cx)
                    {
                        *guest = None;
                        if let Err(error) = outcome.map_err(Error::Task).and_then(|outcome| outcome)
                        {
                            *this = Self::Ended;
                            return Poll::Ready(Some(Err(error)));
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
                    let Self::Reading { guest, .. } = std::mem::replace(this, Self::Ended) else {
                        unreachable!()
                    };
                    match guest {
                        Some(guest) => *this = Self::Waiting { guest, trailers },
                        None => {
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
                    let Self::Waiting { trailers, .. } = std::mem::replace(this, Self::Ended)
                    else {
                        unreachable!()
                    };
                    match outcome {
                        Ok(()) => {
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
