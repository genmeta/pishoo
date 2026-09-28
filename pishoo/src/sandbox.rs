//! Per-identity WASM components, host capabilities, and execution resources.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use dhttp_home::identity::IdentityProfile;
use http::{Method, Uri};
use http_body_util::BodyExt;
use sha2::{Digest, Sha256};
use tokio::task::JoinHandle;
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use wasmtime::{
    Engine, StoreLimits,
    component::{Component, Linker, ResourceTable},
};
use wasmtime_wasi::{WasiCtx, filesystem::WasiFilesystemCtx};
use wasmtime_wasi_http::{WasiHttpCtx, p2::body::HyperOutgoingBody};

use crate::{Error, Result};

mod host;
mod manifest;
mod runtime;

pub use manifest::validate_lib;

pub(crate) struct Sandbox {
    pub(crate) libs: BTreeMap<String, Arc<Lib>>,
    pub(crate) runtime: Arc<WasmRuntime>,
    pub(crate) tasks: TaskTracker,
}

pub(crate) struct WasmRuntime {
    engine: Engine,
    linker: Linker<StoreData>,
}

pub(crate) struct Lib {
    pub(crate) id: String,
    pub(crate) digest: [u8; 32],
    pub(crate) openapi: oas3::OpenApiV3Spec,
    component: Component,
    runtime: Arc<WasmRuntime>,
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
    tasks: TaskTracker,
}

enum LibResponseBody {
    Reading {
        inner: HyperOutgoingBody,
        guest: Option<JoinHandle<Result<()>>>,
    },
    Waiting {
        guest: JoinHandle<Result<()>>,
        trailers: Option<http::HeaderMap>,
    },
    Ended,
}

struct HostOutgoing {
    endpoint: Option<dhttp::Endpoint>,
    policy: LibPolicy,
    children: TaskTracker,
    cancel: CancellationToken,
}

impl Sandbox {
    pub(crate) fn new(runtime: Arc<WasmRuntime>) -> Self {
        Self {
            libs: BTreeMap::new(),
            runtime,
            tasks: TaskTracker::new(),
        }
    }

    /// Close WASM task tracking and cancel every retained version through Lib tokens.
    /// Server separately clears the HTTP router and closes exec task tracking.
    pub(crate) fn close(&mut self) {
        self.tasks.close();
        for lib in self.libs.values() {
            lib.cancel.cancel();
        }
        self.libs.clear();
    }

    /// A timeout leaves unfinished executions owning their Store.
    pub(crate) async fn wait(&self) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(15), self.tasks.wait())
            .await
            .map_err(|_| Error::ShutdownDeadline)
    }

    pub(crate) fn load_libs(&mut self, profile: &IdentityProfile) -> Result<()> {
        let root = profile.join("lib");
        let mut candidates = BTreeMap::new();
        match root.symlink_metadata() {
            Ok(metadata) if !metadata.file_type().is_dir() => {
                return Err(Error::InvalidComponent(
                    "lib root must be a directory, not a symlink".into(),
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                for lib in self.libs.values() {
                    lib.cancel.cancel();
                }
                self.libs.clear();
                return Ok(());
            }
            Err(e) => return Err(e.into()),
            _ => {}
        }
        let entries = std::fs::read_dir(&root)?;
        let mut entries = entries.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let id = entry
                .file_name()
                .into_string()
                .map_err(|_| Error::InvalidComponent("invalid Lib id".into()))?;
            if id.is_empty()
                || id.len() > 63
                || !id.as_bytes()[0].is_ascii_lowercase()
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            {
                return Err(Error::InvalidComponent("invalid Lib id".into()));
            }
            let path = entry.path().join("lib.wasm");
            let metadata = path.symlink_metadata()?;
            if !metadata.file_type().is_file() || metadata.len() > 64 * 1024 * 1024 {
                return Err(Error::InvalidComponent("invalid component file".into()));
            }
            let bytes = std::fs::read(&path)?;
            let digest: [u8; 32] = Sha256::digest(&bytes).into();
            if let Some(lib) = self.libs.get(&id).filter(|lib| lib.digest == digest) {
                candidates.insert(id, lib.clone());
                continue;
            }
            let token = self
                .libs
                .get(&id)
                .map_or_else(CancellationToken::new, |lib| lib.cancel.clone());
            let lib = Lib::load(
                self.runtime.clone(),
                id,
                &bytes,
                &entry.path().join("data"),
                LibPolicy::default(),
                token,
            )?;
            candidates.insert(lib.id.clone(), Arc::new(lib));
        }
        for (id, old) in &self.libs {
            if !candidates.contains_key(id) {
                old.cancel.cancel();
            }
        }
        self.libs = candidates;
        Ok(())
    }

    pub(crate) fn api_router(&self, endpoint: dhttp::Endpoint) -> axum::Router {
        let libs = self.libs.clone();
        let tasks = self.tasks.clone();
        let api = axum::routing::any(move |request: http::Request<axum::body::Body>| {
            let (endpoint, libs, tasks) = (endpoint.clone(), libs.clone(), tasks.clone());
            async move {
                let result: Result<axum::response::Response> = async {
                    let tail = request
                        .uri()
                        .path()
                        .strip_prefix("/api/")
                        .ok_or(Error::RouteNotFound)?;
                    let (id, suffix) = tail
                        .split_once('/')
                        .map_or((tail, "/".to_string()), |(id, p)| (id, format!("/{p}")));
                    let lib = libs.get(id).ok_or(Error::RouteNotFound)?.clone();
                    let item = lib
                        .openapi
                        .paths
                        .as_ref()
                        .and_then(|paths| paths.get(&suffix))
                        .ok_or(Error::RouteNotFound)?;
                    if !item
                        .methods()
                        .into_iter()
                        .any(|(method, _)| method == *request.method())
                    {
                        return Err(Error::MethodNotAllowed);
                    }
                    let handshake = request
                        .extensions()
                        .get::<dhttp::HandshakeSummary>()
                        .ok_or(Error::MissingHandshake)?;
                    let invocation = Invocation::new(lib, endpoint, handshake, tasks)?;
                    let mut request = request.map(|body| body.map_err(Into::into).boxed_unsync());
                    let mut parts = request.uri().clone().into_parts();
                    let path = match request.uri().query() {
                        Some(query) => format!("{suffix}?{query}"),
                        None => suffix,
                    };
                    parts.path_and_query = Some(
                        path.parse()
                            .map_err(|_| Error::BadRequest("invalid API path".into()))?,
                    );
                    *request.uri_mut() = http::Uri::from_parts(parts)
                        .map_err(|_| Error::BadRequest("invalid request URI".into()))?;
                    invocation
                        .execute(request)
                        .await
                        .map(|response| response.map(axum::body::Body::new))
                }
                .await;
                result.unwrap_or_else(reject_api)
            }
        });
        // Reserve every method, including HEAD and OPTIONS, so Axum cannot
        // bypass the manifest's explicit method check or use another fallback.
        axum::Router::new()
            .route("/api", api.clone())
            .route("/api/", api.clone())
            .route("/api/{*path}", api)
    }
}

fn reject_api(error: Error) -> axum::response::Response {
    let status = error.status();
    if status.is_server_error() {
        eprintln!("request failed: {error}");
    }
    axum::response::IntoResponse::into_response((
        status,
        status.canonical_reason().unwrap_or("request failed"),
    ))
}

#[cfg(test)]
#[path = "../tests/unit/sandbox.rs"]
mod tests;
