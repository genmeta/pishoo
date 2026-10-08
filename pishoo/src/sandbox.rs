//! Per-identity WASM components, host capabilities, and execution resources.

use std::{collections::HashMap, sync::Arc, time::Duration};

use dhttp_home::identity::IdentityProfile;
use http_body_util::BodyExt;
use tokio_util::task::TaskTracker;
use wasmtime::{
    Engine, StoreLimits,
    component::{Component, Linker, ResourceTable},
};
use wasmtime_wasi::{WasiCtx, filesystem::WasiFilesystemCtx};
use wasmtime_wasi_http::WasiHttpCtx;

use crate::{Error, Result};

mod host;
mod manifest;
mod runtime;

pub use manifest::validate_lib;

pub(crate) struct Sandbox {
    pub(crate) libs: HashMap<String, Arc<Lib>>,
    pub(crate) runtime: Arc<WasmRuntime>,
    pub(crate) tasks: TaskTracker,
}

pub(crate) struct WasmRuntime {
    engine: Engine,
    linker: Linker<StoreData>,
}

pub(crate) struct Lib {
    pub(crate) openapi: oas3::OpenApiV3Spec,
    component: Component,
    runtime: Arc<WasmRuntime>,
    filesystem: WasiFilesystemCtx,
}

struct StoreData {
    table: ResourceTable,
    wasi: WasiCtx,
    http: WasiHttpCtx,
    memory: StoreLimits,
    deny_outgoing: DenyOutgoing,
    local: dhttp::LocalAuthority,
    remote: Option<dhttp::RemoteAuthority>,
}

pub(crate) struct Invocation {
    lib: Arc<Lib>,
    local: dhttp::LocalAuthority,
    remote: Option<dhttp::RemoteAuthority>,
    tasks: TaskTracker,
}

struct DenyOutgoing;

impl Sandbox {
    pub(crate) fn new(runtime: Arc<WasmRuntime>) -> Self {
        Self {
            libs: HashMap::new(),
            runtime,
            tasks: TaskTracker::new(),
        }
    }

    /// Close WASM task tracking and discard the published Lib set.
    /// Server separately clears the HTTP router.
    pub(crate) fn close(&mut self) {
        self.tasks.close();
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
        let mut candidates = HashMap::new();
        match root.symlink_metadata() {
            Ok(metadata) if !metadata.file_type().is_dir() => {
                return Err(Error::InvalidComponent(
                    "lib root must be a directory, not a symlink".into(),
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
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
            let lib = Lib::load(
                self.runtime.clone(),
                id.clone(),
                &bytes,
                &entry.path().join("data"),
            )?;
            candidates.insert(id, Arc::new(lib));
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
