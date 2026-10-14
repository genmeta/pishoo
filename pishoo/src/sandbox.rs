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

use files::{disk_bytes, lib_root, valid_id};
pub(crate) use files::{
    install_app, install_lib, installed_apps, installed_libs, remove_app, remove_lib,
};
pub use manifest::{validate_lib, validate_openapi};
use package::package_file;
pub(crate) use package::package_metadata;
pub(crate) use package::read_package;
pub(crate) use store::{install_store_app, install_store_lib};

pub(crate) struct Sandbox {
    pub(crate) libs: HashMap<String, Arc<Lib>>,
    pub(crate) apps: HashMap<String, webc::Container>,
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
            apps: HashMap::new(),
            runtime,
            tasks: TaskTracker::new(),
        }
    }

    /// Close WASM task tracking and discard the published Lib set.
    /// Server separately clears the HTTP router.
    pub(crate) fn close(&mut self) {
        self.tasks.close();
        self.libs.clear();
        self.apps.clear();
    }

    /// A timeout leaves unfinished executions owning their Store.
    pub(crate) async fn wait(&self) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(15), self.tasks.wait())
            .await
            .map_err(|_| Error::ShutdownDeadline)
    }

    pub(crate) fn load_libs(&mut self, profile: &IdentityProfile) -> Result<()> {
        let mut candidates = HashMap::new();
        let registered = installed_libs(profile, None)?;
        match lib_root(profile) {
            Ok(root) => {
                rustix::fs::flock(&root, rustix::fs::FlockOperation::LockShared)
                    .map_err(std::io::Error::from)?;
                for record in registered.as_array().unwrap() {
                    if let Some(error) = record["error"].as_str() {
                        return Err(Error::InvalidComponent(error.to_owned()));
                    }
                    let id = record["id"].as_str().unwrap().to_owned();
                    let bytes = disk_bytes(&root, &id)?;
                    let lib = Lib::load(
                        self.runtime.clone(),
                        id.clone(),
                        &bytes,
                        &profile.join("db").join(&id),
                    )?;
                    if record["version"] != lib.openapi.info.version {
                        return Err(Error::InvalidComponent(format!(
                            "registered version changed while loading {id}; retry startup"
                        )));
                    }
                    candidates.insert(id, Arc::new(lib));
                }
            }
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let apps = installed_apps(profile, None)?;
        for package in apps.values() {
            let document = package_metadata(package)?;
            for dependency in document["dependencies"].as_array().unwrap() {
                let id = format!(
                    "{}/{}",
                    dependency["publisher"].as_str().unwrap(),
                    dependency["name"].as_str().unwrap()
                );
                if candidates
                    .get(&id)
                    .is_none_or(|lib| dependency["version"] != lib.openapi.info.version)
                {
                    return Err(Error::InvalidComponent(format!(
                        "app dependency {id} is missing or has a different version"
                    )));
                }
            }
        }
        self.libs = candidates;
        self.apps = apps;
        Ok(())
    }

    pub(crate) fn api_router(
        sandbox: Arc<RwLock<Self>>,
        endpoint: dhttp::Endpoint,
        access: Arc<access_control::AccessService>,
        native_api: axum::Router,
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
        let app_catalog_sandbox = sandbox.clone();
        let app_catalog_endpoint = endpoint.clone();
        let app_catalog = axum::routing::get(move |request: http::Request<axum::body::Body>| {
            let sandbox = app_catalog_sandbox.clone();
            let endpoint = app_catalog_endpoint.clone();
            async move {
                let result: Result<axum::Json<serde_json::Value>> = (|| {
                    let sandbox = sandbox.read().unwrap();
                    require_owner(&endpoint, &request)?;
                    let mut items = sandbox
                        .apps
                        .iter()
                        .map(|(id, package)| Ok(lib_metadata(package, Some(id))))
                        .collect::<Result<Vec<_>>>()?;
                    items.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
                    Ok(axum::Json(items.into()))
                })();
                let mut response = result
                    .map(axum::response::IntoResponse::into_response)
                    .unwrap_or_else(reject_api);
                response.headers_mut().insert(
                    http::header::CACHE_CONTROL,
                    http::HeaderValue::from_static("no-store"),
                );
                response
            }
        });
        let app_sandbox = sandbox.clone();
        let frontend = axum::routing::any(move |request: http::Request<axum::body::Body>| {
            let sandbox = app_sandbox.clone();
            async move {
                let result: Result<axum::response::Response> = (|| {
                    let tail = request
                        .uri()
                        .path()
                        .strip_prefix("/app/")
                        .ok_or(Error::RouteNotFound)?;
                    let name = tail.split('/').next().unwrap();
                    let package = {
                        let sandbox = sandbox.read().unwrap();
                        let id = sandbox
                            .apps
                            .keys()
                            .find(|id| id.rsplit('/').next() == Some(name))
                            .ok_or(Error::RouteNotFound)?;
                        sandbox.apps[id].clone()
                    };
                    let suffix = tail.strip_prefix(name).unwrap();
                    let path = if suffix.is_empty() { "/" } else { suffix };
                    if path == "/index.html" {
                        return Err(Error::RouteNotFound);
                    }
                    let file = if path == "/" {
                        "/public/index.html".to_owned()
                    } else {
                        format!("/public{path}")
                    };
                    let bytes = package_file(&package, &file).map_err(|_| Error::RouteNotFound)?;
                    if !matches!(request.method(), &http::Method::GET | &http::Method::HEAD) {
                        return Err(Error::MethodNotAllowed);
                    }
                    if suffix.is_empty()
                        && matches!(request.method(), &http::Method::GET | &http::Method::HEAD)
                    {
                        let location = match request.uri().query() {
                            Some(query) => format!("{}/?{query}", request.uri().path()),
                            None => format!("{}/", request.uri().path()),
                        };
                        return Ok(axum::response::IntoResponse::into_response(
                            axum::response::Redirect::temporary(&location),
                        ));
                    }
                    let length = bytes.len();
                    http::Response::builder()
                        .status(http::StatusCode::OK)
                        .header(
                            http::header::CONTENT_TYPE,
                            mime_guess::from_path(&file)
                                .first_or_octet_stream()
                                .to_string(),
                        )
                        .header(http::header::CONTENT_LENGTH, length)
                        .header(http::header::CACHE_CONTROL, "no-store")
                        .header("x-content-type-options", "nosniff")
                        .header("referrer-policy", "no-referrer")
                        .header("cross-origin-resource-policy", "same-origin")
                        .header("x-frame-options", "DENY")
                        .body(if request.method() == http::Method::HEAD {
                            axum::body::Body::empty()
                        } else {
                            axum::body::Body::from(bytes)
                        })
                        .map_err(Error::Http)
                })();
                result.unwrap_or_else(reject_api)
            }
        });
        let api = axum::routing::any(move |request: http::Request<axum::body::Body>| {
            let (endpoint, sandbox) = (endpoint.clone(), sandbox.clone());
            let access = access.clone();
            let native_api = native_api.clone();
            async move {
                let result: Result<axum::response::Response> = async {
                    let tail = request
                        .uri()
                        .path()
                        .strip_prefix("/api/")
                        .ok_or(Error::RouteNotFound)?;
                    let name = tail.split('/').next().unwrap();
                    let (lib, tasks) = {
                        let sandbox = sandbox.read().unwrap();
                        let id = sandbox
                            .libs
                            .keys()
                            .find(|id| id.rsplit('/').next() == Some(name))
                            .ok_or(Error::RouteNotFound)?;
                        (sandbox.libs[id].clone(), sandbox.tasks.clone())
                    };
                    // The request owns its exact Lib snapshot before releasing
                    // the collection lock. Package identity stays publisher/name.
                    let suffix = tail.strip_prefix(name).unwrap();
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
                    let _operation = item
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
                    let handshake = request
                        .extensions()
                        .get::<dhttp::HandshakeSummary>()
                        .ok_or(Error::MissingHandshake)?;
                    let invocation = Invocation::new(
                        lib,
                        endpoint.clone(),
                        handshake,
                        tasks,
                        Outgoing::Granted {
                            endpoint,
                            access,
                            api: native_api,
                        },
                    )?;
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
            .route("/std/pishoo/loaded-apps", app_catalog)
            .route("/std/pishoo/loaded-libs", catalog)
            .route("/api/{*path}", api)
            .route("/app/{*path}", frontend)
    }
}

fn list_libs(
    libs: &HashMap<String, Arc<Lib>>,
    endpoint: &dhttp::Endpoint,
    request: &http::Request<axum::body::Body>,
) -> Result<axum::Json<serde_json::Value>> {
    require_owner(endpoint, request)?;
    let mut libs = libs.iter().collect::<Vec<_>>();
    libs.sort_by(|(left, _), (right, _)| left.cmp(right));
    let items = libs
        .into_iter()
        .map(|(id, lib)| lib_metadata(&lib.package, Some(id)))
        .collect::<Vec<_>>();
    Ok(axum::Json(serde_json::json!(items)))
}

fn require_owner(
    endpoint: &dhttp::Endpoint,
    request: &http::Request<axum::body::Body>,
) -> Result<()> {
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
    Ok(())
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

fn lib_metadata(package: &webc::Container, id: Option<&str>) -> serde_json::Value {
    // Callers only pass containers authenticated by read_package.
    let mut value = package_metadata(package).expect("validated package metadata");
    let mut endpoints = Vec::new();
    if value["kind"] == "lib" {
        let openapi =
            validate_openapi(&package_file(package, "/metadata/openapi.json").unwrap()).unwrap();
        if let Some(paths) = &openapi.paths {
            for (path, item) in paths {
                for (method, operation) in item.methods() {
                    let path = id.map_or_else(
                        || path.clone(),
                        |id| format!("/api/{}{path}", id.rsplit('/').next().unwrap()),
                    );
                    endpoints.push(serde_json::json!({"method":method.as_str(),"path":path,
                        "description":operation.summary.as_deref().or(operation.description.as_deref())}));
                }
            }
        }
        value["openapi"] = serde_json::to_value(&openapi).unwrap();
    }
    endpoints.sort_by(|a, b| {
        a["path"]
            .as_str()
            .cmp(&b["path"].as_str())
            .then_with(|| a["method"].as_str().cmp(&b["method"].as_str()))
    });
    value["endpoints"] = endpoints.into();
    if let Some(id) = id {
        value["id"] = id.into();
    }
    value
}

pub(crate) fn check_lib(bytes: &[u8], runtime: &WasmRuntime) -> Result<oas3::OpenApiV3Spec> {
    let package = read_package(bytes)?;
    let openapi = validate_openapi(&package_file(&package, "/metadata/openapi.json")?)?;
    if let Some(backend) = package.get_atom("backend") {
        runtime.compile(&backend)?;
    }
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
        .route("/std/pishoo/apps", any(handler.clone()))
        .route("/std/pishoo/apps/{id}", any(handler.clone()))
        .route("/std/pishoo/apps/{publisher}/{name}", any(handler.clone()))
        .route("/std/pishoo/app-check", any(handler.clone()))
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
    let app = path == "/std/pishoo/app-check"
        || path == "/std/pishoo/apps"
        || path.starts_with("/std/pishoo/apps/");
    let checking = path == "/std/pishoo/lib-check" || path == "/std/pishoo/app-check";
    let store_install = path == "/std/pishoo/store-install";
    let id = path
        .strip_prefix(if app {
            "/std/pishoo/apps/"
        } else {
            "/std/pishoo/libs/"
        })
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
        if input.as_object().is_none_or(|value| {
            !(value.len() == 3 || value.len() == 4 && value.contains_key("kind"))
                || value
                    .keys()
                    .any(|key| !matches!(key.as_str(), "kind" | "publisher" | "name" | "version"))
        }) {
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
        let kind = input
            .get("kind")
            .map(serde_json::Value::as_str)
            .unwrap_or(Some("lib"))
            .ok_or_else(|| Error::BadRequest("invalid artifact kind".into()))?;
        let value = match kind {
            "lib" => {
                install_store_lib(
                    &profile,
                    &endpoint,
                    text("publisher")?,
                    text("name")?,
                    text("version")?,
                    &sandbox,
                )
                .await?
            }
            "app" => {
                install_store_app(
                    &profile,
                    &endpoint,
                    text("publisher")?,
                    text("name")?,
                    text("version")?,
                    &sandbox,
                )
                .await?
            }
            _ => return Err(Error::BadRequest("kind must be app or lib".into())),
        };
        return Ok(axum::Json(value).into_response());
    }
    let runtime = sandbox.read().unwrap().runtime.clone();
    let value = tokio::task::spawn_blocking(move || {
        if app {
            if checking || method == Method::PUT {
                let package = read_package(&bytes)?;
                let document = package_metadata(&package)?;
                if document["kind"] != "app" {
                    return Err(Error::InvalidComponent("expected a frontend app".into()));
                }
                if checking {
                    return Ok(lib_metadata(&package, None));
                }
                let id = id.as_deref().unwrap();
                let mut sandbox = sandbox.write().unwrap();
                if sandbox.tasks.is_closed() {
                    return Err(Error::Denied);
                }
                for dependency in document["dependencies"].as_array().unwrap() {
                    let dep_id = format!(
                        "{}/{}",
                        dependency["publisher"].as_str().unwrap(),
                        dependency["name"].as_str().unwrap()
                    );
                    if sandbox
                        .libs
                        .get(&dep_id)
                        .is_none_or(|lib| dependency["version"] != lib.openapi.info.version)
                    {
                        return Err(Error::Io(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "app dependency is not loaded at its required version",
                        )));
                    }
                }
                let value = install_app(&profile, id, &bytes)?;
                sandbox.apps.insert(id.to_owned(), package);
                return Ok(value);
            }
            if method == Method::DELETE {
                let mut sandbox = sandbox.write().unwrap();
                if sandbox.tasks.is_closed() {
                    return Err(Error::Denied);
                }
                let id = id.as_deref().unwrap();
                remove_app(&profile, id)?;
                sandbox.apps.remove(id);
                return Ok(serde_json::Value::Null);
            }
            let apps = installed_apps(&profile, id.as_deref())?;
            let mut values = apps
                .iter()
                .map(|(id, package)| Ok(lib_metadata(package, Some(id))))
                .collect::<Result<Vec<_>>>()?;
            values.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
            return if id.is_some() {
                values.pop().ok_or(Error::RouteNotFound)
            } else {
                Ok(values.into())
            };
        }
        if checking {
            check_lib(&bytes, &runtime)?;
            return Ok(lib_metadata(&read_package(&bytes)?, None));
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
                let metadata = package_metadata(&lib.package)?;
                for dependency in metadata["dependencies"].as_array().unwrap() {
                    let dependency_id = format!(
                        "{}/{}",
                        dependency["publisher"].as_str().unwrap(),
                        dependency["name"].as_str().unwrap()
                    );
                    if sandbox
                        .libs
                        .get(&dependency_id)
                        .is_none_or(|lib| dependency["version"] != lib.openapi.info.version)
                    {
                        return Err(Error::Io(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "Lib dependency is not loaded at its required version",
                        )));
                    }
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
