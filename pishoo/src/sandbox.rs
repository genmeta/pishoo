//! Per-identity WASM components, host capabilities, and execution resources.

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::Duration,
};

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
mod package;
mod runtime;
mod store;

use files::{disk_bytes, disk_ids, lib_root, valid_id};
pub(crate) use files::{install_lib, installed_libs, remove_lib};
pub use manifest::{validate_lib, validate_openapi};
use package::{package_file, read_package};
pub(crate) use store::install_store_lib;

pub(crate) struct Sandbox {
    pub(crate) libs: HashMap<String, Arc<Lib>>,
    pub(crate) runtime: Arc<WasmRuntime>,
    pub(crate) tasks: TaskTracker,
}

pub struct WasmRuntime {
    engine: Engine,
    linker: Linker<StoreData>,
}

pub(crate) struct Lib {
    pub(crate) openapi: oas3::OpenApiV3Spec,
    component: Component,
    package: webc::Container,
    runtime: Arc<WasmRuntime>,
    filesystem: WasiFilesystemCtx,
}

struct StoreData {
    table: ResourceTable,
    wasi: WasiCtx,
    http: WasiHttpCtx,
    memory: StoreLimits,
    outgoing: Outgoing,
    local: dhttp::LocalAuthority,
    remote: Option<dhttp::RemoteAuthority>,
}

pub(crate) struct Invocation {
    lib: Arc<Lib>,
    local: dhttp::LocalAuthority,
    remote: Option<dhttp::RemoteAuthority>,
    tasks: TaskTracker,
    outgoing: Outgoing,
}

pub(crate) enum Outgoing {
    Denied,
    Granted {
        endpoint: dhttp::Endpoint,
        access: Arc<access_control::AccessService>,
        api: axum::Router,
    },
}

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

    pub(crate) fn api_router(
        sandbox: Arc<RwLock<Self>>,
        endpoint: dhttp::Endpoint,
    ) -> axum::Router {
        let catalog_sandbox = sandbox.clone();
        let catalog_endpoint = endpoint.clone();
        let catalog = axum::routing::get(move |request: http::Request<axum::body::Body>| {
            let (sandbox, endpoint) = (catalog_sandbox.clone(), catalog_endpoint.clone());
            async move {
                use axum::response::IntoResponse;
                let result = {
                    let sandbox = sandbox.read().unwrap();
                    list_libs(&sandbox.libs, &endpoint, &request)
                };
                let mut response = result
                    .map(IntoResponse::into_response)
                    .unwrap_or_else(reject_api);
                response.headers_mut().insert(
                    http::header::CACHE_CONTROL,
                    http::HeaderValue::from_static("no-store"),
                );
                response
            }
        });
        let api = axum::routing::any(move |request: http::Request<axum::body::Body>| {
            let (endpoint, sandbox) = (endpoint.clone(), sandbox.clone());
            async move {
                let result: Result<axum::response::Response> = async {
                    let tail = request
                        .uri()
                        .path()
                        .strip_prefix("/std/api/")
                        .ok_or(Error::RouteNotFound)?;
                    let (id, lib, tasks) = {
                        let sandbox = sandbox.read().unwrap();
                        let id = sandbox
                            .libs
                            .keys()
                            .find(|id| {
                                tail == id.as_str()
                                    || tail
                                        .strip_prefix(id.as_str())
                                        .is_some_and(|suffix| suffix.starts_with('/'))
                            })
                            .ok_or(Error::RouteNotFound)?;
                        (id.clone(), sandbox.libs[id].clone(), sandbox.tasks.clone())
                    };
                    // The request owns its exact Lib snapshot before releasing
                    // the collection lock. Updates cannot mix frontend/backend.
                    let suffix = tail.strip_prefix(id.as_str()).unwrap();
                    let at_root = suffix.is_empty();
                    let suffix = if at_root {
                        "/".to_owned()
                    } else {
                        suffix.to_owned()
                    };
                    let item = lib
                        .openapi
                        .paths
                        .as_ref()
                        .and_then(|paths| paths.get(&suffix))
                        .ok_or(Error::RouteNotFound)?;
                    let operation = item
                        .methods()
                        .into_iter()
                        .find(|(method, _)| method == *request.method())
                        .map(|(_, operation)| operation)
                        .ok_or(Error::MethodNotAllowed)?;
                    // Canonicalize only declared page roots so browser-relative
                    // URLs stay inside this Lib's mount, regardless of its name.
                    if at_root
                        && (request.method() == http::Method::GET
                            || request.method() == http::Method::HEAD)
                    {
                        let location = match request.uri().query() {
                            Some(query) => format!("{}/?{query}", request.uri().path()),
                            None => format!("{}/", request.uri().path()),
                        };
                        return Ok(axum::response::IntoResponse::into_response(
                            axum::response::Redirect::temporary(&location),
                        ));
                    }
                    // Static operations read the same immutable package snapshot as
                    // this Lib's backend. Server applies daccess to both paths.
                    let operation = serde_json::to_value(operation)
                        .map_err(|error| Error::InvalidComponent(error.to_string()))?;
                    if let Some(path) = operation["x-pishoo"]["static-file"].as_str() {
                        let bytes = package_file(&lib.package, path)?;
                        let content_type = mime_guess::from_path(path)
                            .first_or_octet_stream()
                            .to_string();
                        let length = bytes.len();
                        let body = if request.method() == http::Method::HEAD {
                            axum::body::Body::empty()
                        } else {
                            axum::body::Body::from(bytes)
                        };
                        return http::Response::builder()
                            .status(http::StatusCode::OK)
                            .header(http::header::CONTENT_TYPE, content_type)
                            .header(http::header::CONTENT_LENGTH, length)
                            .header(http::header::CACHE_CONTROL, "no-store")
                            .header("x-content-type-options", "nosniff")
                            .header("referrer-policy", "no-referrer")
                            .header("cross-origin-resource-policy", "same-origin")
                            .header("x-frame-options", "DENY")
                            .body(body)
                            .map_err(Error::Http);
                    }
                    let handshake = request
                        .extensions()
                        .get::<dhttp::HandshakeSummary>()
                        .ok_or(Error::MissingHandshake)?;
                    let invocation =
                        Invocation::new(lib, endpoint, handshake, tasks, Outgoing::Denied)?;
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
        // The reserved namespace is fixed; Lib lookup changes immediately with
        // the shared collection, without rebuilding the Server router.
        axum::Router::new()
            .route("/std/workspace-api/libs", catalog)
            .route("/std/api/{*path}", api)
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
                endpoints.push(serde_json::json!({"method":method.as_str(), "path":id.map_or_else(|| path.clone(), |id| format!("/std/api/{id}{path}")), "description":description}));
            }
        }
    }
    endpoints.sort_by(|a, b| {
        a["path"]
            .as_str()
            .cmp(&b["path"].as_str())
            .then_with(|| a["method"].as_str().cmp(&b["method"].as_str()))
    });
    let mut value = serde_json::json!({"title":openapi.info.title, "version":openapi.info.version, "description":openapi.info.description, "endpoints":endpoints, "openapi":openapi});
    if let Some(id) = id {
        value["id"] = id.into();
    }
    value
}

pub(crate) fn check_lib(bytes: &[u8], runtime: &WasmRuntime) -> Result<oas3::OpenApiV3Spec> {
    let package = read_package(bytes)?;
    let openapi = validate_openapi(&package_file(&package, "/metadata/openapi.json")?)?;
    runtime.compile(&package.get_atom("backend").unwrap())?;
    Ok(openapi)
}

pub(crate) fn lib_management_router(
    profile: IdentityProfile,
    endpoint: dhttp::Endpoint,
    sandbox: Arc<RwLock<Sandbox>>,
) -> axum::Router {
    use axum::{response::IntoResponse, routing::any};
    let handler = move |request: http::Request<axum::body::Body>| {
        let (profile, endpoint, sandbox) = (profile.clone(), endpoint.clone(), sandbox.clone());
        async move {
            let mut response = lib_management_request(profile, endpoint, sandbox, request)
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
        .route("/std/pishoo/libs", any(handler.clone()))
        .route("/std/pishoo/libs/{id}", any(handler.clone()))
        .route("/std/pishoo/libs/{publisher}/{name}", any(handler.clone()))
        .route("/std/pishoo/lib-check", any(handler.clone()))
        .route("/std/pishoo/store-install", any(handler))
}

async fn lib_management_request(
    profile: IdentityProfile,
    endpoint: dhttp::Endpoint,
    sandbox: Arc<RwLock<Sandbox>>,
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
    let checking = path == "/std/pishoo/lib-check";
    let store_install = path == "/std/pishoo/store-install";
    let id = path
        .strip_prefix("/std/pishoo/libs/")
        .map(|id| {
            id.split('/')
                .map(|part| {
                    let part = percent_encoding::percent_decode_str(part)
                        .decode_utf8()
                        .map_err(|_| Error::BadRequest("invalid id encoding".into()))?;
                    if part.contains('/') {
                        return Err(Error::BadRequest("encoded path separator in Lib ID".into()));
                    }
                    Ok(part.into_owned())
                })
                .collect::<Result<Vec<_>>>()
                .map(|parts| parts.join("/"))
        })
        .transpose()?;
    if let Some(id) = &id {
        valid_id(id)?;
    }
    let method = request.method().clone();
    let allow = if checking || store_install {
        "POST"
    } else if id.is_none() {
        "GET"
    } else {
        "GET, PUT, DELETE"
    };
    if !((checking || store_install) && method == Method::POST
        || !checking && !store_install && method == Method::GET
        || id.is_some() && (method == Method::PUT || method == Method::DELETE))
    {
        return Ok((StatusCode::METHOD_NOT_ALLOWED, [(header::ALLOW, allow)]).into_response());
    }
    if request.uri().query().is_some() {
        return Err(Error::BadRequest("unsupported query parameters".into()));
    }
    let writing = method == Method::PUT || checking || store_install;
    if writing
        && !request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(';').next())
            .is_some_and(|v| {
                v.trim().eq_ignore_ascii_case(if store_install {
                    "application/json"
                } else {
                    "application/webc"
                })
            })
    {
        return Ok(StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response());
    }
    if request.headers().contains_key(header::TRAILER) {
        return Err(Error::BadRequest(
            "Lib management trailers are unsupported".into(),
        ));
    }
    let bytes = match tokio::time::timeout(
        Duration::from_secs(if store_install { 15 } else { 120 }),
        axum::body::to_bytes(
            request.into_body(),
            if store_install {
                4096
            } else if writing {
                64 * 1024 * 1024
            } else {
                0
            },
        ),
    )
    .await
    .map_err(|_| Error::Deadline)?
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
            return Err(Error::BadRequest("failed to read WEBC body".into()));
        }
    };
    if store_install {
        let input: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|_| Error::BadRequest("invalid store installation JSON".into()))?;
        if input.as_object().is_none_or(|value| value.len() != 3) {
            return Err(Error::BadRequest(
                "store installation requires only publisher, name and version".into(),
            ));
        }
        let text = |key: &str| {
            input
                .get(key)
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| Error::BadRequest(format!("missing {key}")))
        };
        let value = install_store_lib(
            &profile,
            &endpoint,
            text("publisher")?,
            text("name")?,
            text("version")?,
            &sandbox,
        )
        .await?;
        return Ok(axum::Json(value).into_response());
    }
    let runtime = sandbox.read().unwrap().runtime.clone();
    let value = tokio::task::spawn_blocking(move || {
        if checking {
            return check_lib(&bytes, &runtime).map(|api| lib_metadata(&api, None));
        }
        match method {
            Method::GET => installed_libs(&profile, id.as_deref()),
            Method::PUT => {
                let id = id.as_deref().unwrap();
                // Prepare the full executable/resource snapshot before commit.
                let lib = Lib::load(
                    runtime.clone(),
                    id.to_owned(),
                    &bytes,
                    &profile.join("db").join(id),
                )?;
                let mut sandbox = sandbox.write().unwrap();
                if sandbox.tasks.is_closed() {
                    return Err(Error::Denied);
                }
                let value = install_lib(&profile, id, &bytes, &runtime)?;
                sandbox.libs.insert(id.to_owned(), Arc::new(lib));
                Ok(value)
            }
            Method::DELETE => {
                let mut sandbox = sandbox.write().unwrap();
                if sandbox.tasks.is_closed() {
                    return Err(Error::Denied);
                }
                let id = id.as_deref().unwrap();
                remove_lib(&profile, id)?;
                sandbox.libs.remove(id);
                Ok(serde_json::Value::Null)
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
pub(crate) mod tests;
