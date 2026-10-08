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

mod files;
mod host;
mod manifest;
mod runtime;

use files::{disk_bytes, disk_ids, lib_root, valid_id};
pub(crate) use files::{install_lib, installed_libs, remove_lib};
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
        let root = match lib_root(profile) {
            Ok(root) => root,
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                self.libs.clear();
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        rustix::fs::flock(&root, rustix::fs::FlockOperation::LockShared)
            .map_err(std::io::Error::from)?;
        let mut candidates = HashMap::new();
        for id in disk_ids(&root)? {
            let bytes = disk_bytes(&root, &id)?;
            let lib = Lib::load(
                self.runtime.clone(),
                id.clone(),
                &bytes,
                &profile.join("db").join(&id),
            )?;
            candidates.insert(id, Arc::new(lib));
        }
        self.libs = candidates;
        Ok(())
    }

    pub(crate) fn api_router(&self, endpoint: dhttp::Endpoint) -> axum::Router {
        let catalog_libs = self.libs.clone();
        let catalog_endpoint = endpoint.clone();
        let catalog = axum::routing::get(move |request: http::Request<axum::body::Body>| {
            let (libs, endpoint) = (catalog_libs.clone(), catalog_endpoint.clone());
            async move {
                use axum::response::IntoResponse;
                let mut response = list_libs(&libs, &endpoint, &request)
                    .map(IntoResponse::into_response)
                    .unwrap_or_else(reject_api);
                response.headers_mut().insert(
                    http::header::CACHE_CONTROL,
                    http::HeaderValue::from_static("no-store"),
                );
                response
            }
        });
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
            .route("/workspace-api/libs", catalog)
            .route("/api", api.clone())
            .route("/api/", api.clone())
            .route("/api/{*path}", api)
    }
}

fn list_libs(
    libs: &HashMap<String, Arc<Lib>>,
    endpoint: &dhttp::Endpoint,
    request: &http::Request<axum::body::Body>,
) -> Result<axum::Json<serde_json::Value>> {
    // Server's authorization layer creates Visitor from the verified TLS peer.
    let visitor = request
        .extensions()
        .get::<access_control::Visitor>()
        .ok_or(Error::Denied)?;
    let local = endpoint.local_authority()?;
    let ski = dhttp_home::certificate::extract_dhttp_subject_key_identifier(local.certificates())
        .map_err(|_| Error::Denied)?;
    let subject = access_control::SubjectId::new(ski.owner_hash().as_str().as_bytes())
        .map_err(|_| Error::Denied)?;
    if visitor.name() != endpoint.name() || visitor.subject_id() != &subject {
        return Err(Error::Denied);
    }
    let mut libs = libs.iter().collect::<Vec<_>>();
    libs.sort_by(|(left, _), (right, _)| left.cmp(right));
    let items = libs
        .into_iter()
        .map(|(id, lib)| lib_metadata(&lib.openapi, Some(id)))
        .collect::<Vec<_>>();
    Ok(axum::Json(serde_json::json!(items)))
}

fn reject_api(error: Error) -> axum::response::Response {
    let status = error.status();
    if status.is_server_error() {
        tracing::error!(%error, "request failed");
    }
    axum::response::IntoResponse::into_response((
        status,
        status.canonical_reason().unwrap_or("request failed"),
    ))
}

fn lib_metadata(openapi: &oas3::OpenApiV3Spec, id: Option<&str>) -> serde_json::Value {
    let mut endpoints = Vec::new();
    if let Some(paths) = &openapi.paths {
        for (path, item) in paths {
            for (method, operation) in item.methods() {
                let description = operation
                    .summary
                    .as_deref()
                    .filter(|text| !text.trim().is_empty())
                    .or_else(|| {
                        operation
                            .description
                            .as_deref()
                            .filter(|text| !text.trim().is_empty())
                    })
                    .map(str::to_owned)
                    .or_else(|| {
                        operation
                            .responses
                            .as_ref()?
                            .iter()
                            .find_map(|(status, response)| {
                                if status != "2XX"
                                    && !status
                                        .parse::<u16>()
                                        .is_ok_and(|code| (200..300).contains(&code))
                                {
                                    return None;
                                }
                                response
                                    .resolve(openapi)
                                    .ok()?
                                    .description
                                    .filter(|text| !text.trim().is_empty())
                            })
                    });
                endpoints.push(serde_json::json!({"method":method.as_str(), "path":id.map_or_else(|| path.clone(), |id| format!("/api/{id}{path}")), "description":description}));
            }
        }
    }
    endpoints.sort_by(|a, b| {
        a["path"]
            .as_str()
            .cmp(&b["path"].as_str())
            .then_with(|| a["method"].as_str().cmp(&b["method"].as_str()))
    });
    let mut value = serde_json::json!({"title":openapi.info.title, "version":openapi.info.version, "description":openapi.info.description, "endpoints":endpoints});
    if let Some(id) = id {
        value["id"] = id.into();
    }
    value
}

pub(crate) fn check_lib(bytes: &[u8], runtime: &WasmRuntime) -> Result<oas3::OpenApiV3Spec> {
    let openapi = validate_lib(bytes)?;
    runtime.compile(bytes)?;
    Ok(openapi)
}

pub(crate) fn lib_management_router(
    profile: IdentityProfile,
    endpoint: dhttp::Endpoint,
    runtime: Arc<WasmRuntime>,
) -> axum::Router {
    use axum::{response::IntoResponse, routing::any};
    let handler = move |request: http::Request<axum::body::Body>| {
        let (profile, endpoint, runtime) = (profile.clone(), endpoint.clone(), runtime.clone());
        async move {
            let mut response = lib_management_request(profile, endpoint, runtime, request)
                .await
                .unwrap_or_else(|error| {
                    let status = match &error {
                        Error::InvalidComponent(_) => http::StatusCode::BAD_REQUEST,
                        Error::Io(io) if io.kind() == std::io::ErrorKind::InvalidInput => {
                            http::StatusCode::CONFLICT
                        }
                        _ => error.status(),
                    };
                    if status.is_server_error() {
                        tracing::error!(%error, "Lib management failed");
                        (status, "Lib storage or compilation failed; query the resource to confirm the saved result").into_response()
                    } else {
                        (status, error.to_string()).into_response()
                    }
                });
            response
                .headers_mut()
                .insert(http::header::CACHE_CONTROL, "no-store".parse().unwrap());
            response
                .headers_mut()
                .insert("supported-versions", "v1".parse().unwrap());
            response
        }
    };
    axum::Router::new()
        .route("/pishoo/libs", any(handler.clone()))
        .route("/pishoo/libs/{id}", any(handler.clone()))
        .route("/pishoo/lib-check", any(handler))
}

async fn lib_management_request(
    profile: IdentityProfile,
    endpoint: dhttp::Endpoint,
    runtime: Arc<WasmRuntime>,
    request: http::Request<axum::body::Body>,
) -> Result<axum::response::Response> {
    use axum::response::IntoResponse;
    use http::{Method, StatusCode, header};
    let visitor = request
        .extensions()
        .get::<access_control::Visitor>()
        .ok_or(Error::Denied)?;
    let local = endpoint.local_authority()?;
    let ski = dhttp_home::certificate::extract_dhttp_subject_key_identifier(local.certificates())
        .map_err(|_| Error::Denied)?;
    let subject = access_control::SubjectId::new(ski.owner_hash().as_str().as_bytes())
        .map_err(|_| Error::Denied)?;
    if visitor.name() != endpoint.name() || visitor.subject_id() != &subject {
        return Err(Error::Denied);
    }
    if request.headers().contains_key("accept-versions") {
        let mut supported = false;
        for value in request.headers().get_all("accept-versions") {
            supported |= value
                .to_str()
                .map_err(|_| Error::BadRequest("invalid Accept-Versions".into()))?
                .split(',')
                .any(|v| v.trim() == "v1");
        }
        if !supported {
            return Ok(StatusCode::HTTP_VERSION_NOT_SUPPORTED.into_response());
        }
    }
    let path = request.uri().path();
    let checking = path == "/pishoo/lib-check";
    let id = path
        .strip_prefix("/pishoo/libs/")
        .map(|id| {
            percent_encoding::percent_decode_str(id)
                .decode_utf8()
                .map(|s| s.into_owned())
                .map_err(|_| Error::BadRequest("invalid id encoding".into()))
        })
        .transpose()?;
    if let Some(id) = &id {
        valid_id(id)?;
    }
    let method = request.method().clone();
    let allow = if checking {
        "POST"
    } else if id.is_none() {
        "GET"
    } else {
        "GET, PUT, DELETE"
    };
    if !(checking && method == Method::POST
        || !checking && method == Method::GET
        || id.is_some() && (method == Method::PUT || method == Method::DELETE))
    {
        return Ok((StatusCode::METHOD_NOT_ALLOWED, [(header::ALLOW, allow)]).into_response());
    }
    if request.uri().query().is_some() {
        return Err(Error::BadRequest("unsupported query parameters".into()));
    }
    let writing = method == Method::PUT || checking;
    if writing
        && !request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(';').next())
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/wasm"))
    {
        return Ok(StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response());
    }
    let bytes = match axum::body::to_bytes(
        request.into_body(),
        if writing { 64 * 1024 * 1024 } else { 0 },
    )
    .await
    {
        Ok(bytes) => bytes,
        Err(error) => {
            use std::error::Error as _;
            if error
                .source()
                .is_some_and(|source| source.is::<http_body_util::LengthLimitError>())
            {
                return Ok(if writing {
                    StatusCode::PAYLOAD_TOO_LARGE
                } else {
                    StatusCode::BAD_REQUEST
                }
                .into_response());
            }
            return Err(Error::BadRequest("failed to read component body".into()));
        }
    };
    let value = tokio::task::spawn_blocking(move || {
        if checking {
            return check_lib(&bytes, &runtime).map(|api| lib_metadata(&api, None));
        }
        match method {
            Method::GET => installed_libs(&profile, id.as_deref()),
            Method::PUT => install_lib(&profile, id.as_deref().unwrap(), &bytes, &runtime),
            Method::DELETE => {
                remove_lib(&profile, id.as_deref().unwrap()).map(|()| serde_json::Value::Null)
            }
            _ => Err(Error::MethodNotAllowed),
        }
    })
    .await??;
    Ok(if value.is_null() {
        StatusCode::NO_CONTENT.into_response()
    } else {
        axum::Json(value).into_response()
    })
}

#[cfg(test)]
#[path = "../tests/unit/sandbox.rs"]
mod tests;
