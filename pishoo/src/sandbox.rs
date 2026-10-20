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
pub(crate) use package::{package_metadata, read_package};
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
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "frozen outgoing policy used by isolation tests")
    )]
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
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "frozen shutdown operation used by resource tests")
    )]
    pub(crate) async fn wait(&self) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(15), self.tasks.wait())
            .await
            .map_err(|_| Error::ShutdownDeadline)
    }

    pub(crate) fn load_libs(&mut self, profile: &IdentityProfile) -> Result<()> {
        let parent = std::fs::File::from(
            rustix::fs::open(
                profile.path(),
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::DIRECTORY
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )
            .map_err(std::io::Error::from)?,
        );
        rustix::fs::flock(&parent, rustix::fs::FlockOperation::LockShared)
            .map_err(std::io::Error::from)?;
        let mut candidates = HashMap::new();
        // Startup selects registered IDs, then reads each executable once.
        // Package integrity, dependencies and route conflicts were checked when
        // installation committed these rows and files together.
        let database = profile.config_db_path();
        if !database.symlink_metadata()?.file_type().is_file() {
            return Err(Error::InvalidConfig(
                "config.db must be a regular file".into(),
            ));
        }
        let conn = rusqlite::Connection::open_with_flags(
            database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        conn.busy_timeout(Duration::from_secs(3))?;
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='libs')",
            [],
            |row| row.get(0),
        )?;
        let registered = if exists {
            conn.prepare("SELECT id FROM libs ORDER BY id")?
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        } else {
            Vec::new()
        };
        match lib_root(profile) {
            Ok(root) => {
                rustix::fs::flock(&root, rustix::fs::FlockOperation::LockShared)
                    .map_err(std::io::Error::from)?;
                for id in &registered {
                    let bytes = disk_bytes(&root, &id)?;
                    let lib = Lib::load(
                        self.runtime.clone(),
                        id.clone(),
                        &bytes,
                        &profile.join("db").join(&id),
                    )?;
                    candidates.insert(id.clone(), Arc::new(lib));
                }
            }
            Err(Error::Io(error))
                if error.kind() == std::io::ErrorKind::NotFound && registered.is_empty() => {}
            Err(error) => return Err(error),
        }
        let apps = installed_apps(profile, None)?;
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
        let api = axum::routing::any(move |request: http::Request<axum::body::Body>| {
            let (endpoint, sandbox, access, native_api) = (
                endpoint.clone(),
                sandbox.clone(),
                access.clone(),
                native_api.clone(),
            );
            async move {
                let result: Result<axum::response::Response> = async {
                    let path = request
                        .uri()
                        .path()
                        .strip_prefix("/std")
                        .ok_or(Error::RouteNotFound)?
                        .to_owned();
                    // One exact snapshot, then release the shared collection lock.
                    let (lib, tasks, frontend) = {
                        let sandbox = sandbox.read().unwrap();
                        let lib = sandbox
                            .libs
                            .values()
                            .find(|lib| {
                                lib.openapi
                                    .paths
                                    .as_ref()
                                    .is_some_and(|paths| paths.contains_key(&path))
                            })
                            .cloned();
                        let tail = path.trim_start_matches('/');
                        let name = tail.split('/').next().unwrap();
                        let frontend = sandbox
                            .apps
                            .iter()
                            .find(|(id, _)| id.rsplit('/').next() == Some(name))
                            .and_then(|(_, package)| {
                                let suffix = tail.strip_prefix(name).unwrap();
                                let file = if suffix.is_empty() || suffix == "/" {
                                    "/public/index.html".to_owned()
                                } else {
                                    format!("/public{suffix}")
                                };
                                if suffix == "/index.html" {
                                    return None;
                                }
                                package_file(package, &file)
                                    .ok()
                                    .map(|bytes| (file, bytes, suffix.is_empty()))
                            });
                        (lib, sandbox.tasks.clone(), frontend)
                    };
                    if let Some((file, bytes, redirect)) = &frontend
                        && matches!(request.method(), &http::Method::GET | &http::Method::HEAD)
                    {
                        if *redirect {
                            let location = match request.uri().query() {
                                Some(query) => format!("{}/?{query}", request.uri().path()),
                                None => format!("{}/", request.uri().path()),
                            };
                            return Ok(axum::response::IntoResponse::into_response(
                                axum::response::Redirect::temporary(&location),
                            ));
                        }
                        return http::Response::builder()
                            .status(http::StatusCode::OK)
                            .header(
                                http::header::CONTENT_TYPE,
                                mime_guess::from_path(file)
                                    .first_or_octet_stream()
                                    .to_string(),
                            )
                            .header(http::header::CONTENT_LENGTH, bytes.len())
                            .header(http::header::CACHE_CONTROL, "no-store")
                            .header("x-content-type-options", "nosniff")
                            .header("referrer-policy", "no-referrer")
                            .header("cross-origin-resource-policy", "same-origin")
                            .header("x-frame-options", "DENY")
                            .body(if request.method() == http::Method::HEAD {
                                axum::body::Body::empty()
                            } else {
                                axum::body::Body::from(bytes.clone())
                            })
                            .map_err(Error::Http);
                    }
                    let Some(lib) = lib else {
                        return Err(if frontend.is_some() {
                            Error::MethodNotAllowed
                        } else {
                            Error::RouteNotFound
                        });
                    };
                    let item = &lib.openapi.paths.as_ref().unwrap()[&path];
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
                    parts.path_and_query = Some(
                        match request.uri().query() {
                            Some(query) => format!("{path}?{query}"),
                            None => path,
                        }
                        .parse()
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
        axum::Router::new()
            .route("/std/sys/package/loaded-apps", app_catalog)
            .route("/std/sys/package/loaded-libs", catalog)
            .route(
                "/std",
                axum::routing::any(|| async { reject_api(Error::RouteNotFound) }),
            )
            .route("/std/{*path}", api)
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
    // Installed packages are trusted after installation. Catalog reads parse
    // display metadata without repeating package or OpenAPI policy validation.
    let mut value = match package_metadata(package) {
        Ok(value) => value,
        Err(error) => return serde_json::json!({"id":id,"error":error.to_string()}),
    };
    let mut endpoints = Vec::new();
    if value["kind"] == "lib" {
        let openapi: oas3::OpenApiV3Spec = match package_file(package, "/metadata/openapi.json")
            .and_then(|bytes| {
                serde_json::from_slice(&bytes)
                    .map_err(|error| Error::InvalidComponent(error.to_string()))
            }) {
            Ok(openapi) => openapi,
            Err(error) => return serde_json::json!({"id":id,"error":error.to_string()}),
        };
        if let Some(paths) = &openapi.paths {
            for (path, item) in paths {
                for (method, operation) in item.methods() {
                    let path = format!("/std{path}");
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

// HTTP catalog reads installation facts, with display details from the already
// loaded package at that version. It never opens payloads or waits for their
// file locks. Offline changes remain visible through their database records.
fn installed_catalog(
    profile: &IdentityProfile,
    sandbox: &Sandbox,
    app: bool,
    name: Option<&str>,
) -> Result<serde_json::Value> {
    let path = profile.config_db_path();
    if !path.symlink_metadata()?.file_type().is_file() {
        return Err(Error::InvalidConfig(
            "config.db must be a regular file".into(),
        ));
    }
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(Duration::from_secs(3))?;
    let table = if app { "apps" } else { "libs" };
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |row| row.get(0),
    )?;
    let mut items = Vec::new();
    if exists {
        let mut stmt = conn.prepare(&format!(
            "SELECT id,name,version FROM {table} WHERE ?1 IS NULL OR name=?1 ORDER BY id"
        ))?;
        for row in stmt.query_map([name], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })? {
            let (id, name, version) = row?;
            let package = if app {
                sandbox.apps.get(&id)
            } else {
                sandbox.libs.get(&id).map(|lib| &lib.package)
            };
            let mut value = package
                .map(|package| lib_metadata(package, Some(&id)))
                .filter(|value| value["version"] == version)
                .unwrap_or_else(|| {
                    serde_json::json!({
                        "id":id, "title":name, "version":version,
                        "kind":if app {"app"} else {"lib"},
                        "description":null, "endpoints":[],
                    })
                });
            let (edges, source, target) = if app {
                ("app_dependencies", "app_id", "lib_id")
            } else {
                ("lib_dependencies", "lib_id", "dependency_id")
            };
            let dependencies = conn
                .prepare(&format!(
                    "SELECT {target},version FROM {edges} WHERE {source}=?1 ORDER BY {target}"
                ))?
                .query_map([&id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .map(|row| {
                    let (id, version) = row?;
                    let (publisher, name) = id.split_once('/').ok_or_else(|| {
                        Error::InvalidConfig("invalid registered dependency ID".into())
                    })?;
                    Ok(serde_json::json!({"publisher":publisher,"name":name,"version":version}))
                })
                .collect::<Result<Vec<_>>>()?;
            value["dependencies"] = dependencies.into();
            items.push(value);
        }
    }
    if name.is_some() {
        items.pop().ok_or(Error::RouteNotFound)
    } else {
        Ok(items.into())
    }
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
        .route("/std/sys/package/libs", any(handler.clone()))
        .route("/std/sys/package/libs/{name}", any(handler.clone()))
        .route("/std/sys/package/apps", any(handler.clone()))
        .route("/std/sys/package/apps/{name}", any(handler.clone()))
        .route("/std/sys/package/app-check", any(handler.clone()))
        .route("/std/sys/package/lib-check", any(handler.clone()))
        .route("/std/sys/package/store-install", any(handler))
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
    let app = path == "/std/sys/package/app-check"
        || path == "/std/sys/package/apps"
        || path.starts_with("/std/sys/package/apps/");
    let checking = path == "/std/sys/package/lib-check" || path == "/std/sys/package/app-check";
    let store_install = path == "/std/sys/package/store-install";
    let id = path
        .strip_prefix(if app {
            "/std/sys/package/apps/"
        } else {
            "/std/sys/package/libs/"
        })
        .map(|name| {
            let name = percent_encoding::percent_decode_str(name)
                .decode_utf8()
                .map_err(|_| Error::BadRequest("invalid package name encoding".into()))?;
            if name.contains('/') {
                return Err(Error::BadRequest("path separator in package name".into()));
            }
            Ok(name.into_owned())
        })
        .transpose()?;
    if let Some(id) = &id {
        valid_id(id)?;
    }
    let method = request.method().clone();
    let expected_id = request.headers().get("expected-package-id").cloned();
    if expected_id.is_some() && (method != Method::DELETE || id.is_none()) {
        return Err(Error::BadRequest(
            "Expected-Package-Id is supported only for package removal".into(),
        ));
    }
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
                let media_type = v.trim();
                if store_install {
                    media_type.eq_ignore_ascii_case("application/json")
                } else {
                    // genmeta preview.3 uploads WEBC as generic binary data.
                    media_type.eq_ignore_ascii_case("application/webc")
                        || media_type.eq_ignore_ascii_case("application/octet-stream")
                }
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
    if method == Method::GET {
        let value = tokio::task::spawn_blocking(move || {
            installed_catalog(&profile, &sandbox.read().unwrap(), app, id.as_deref())
        })
        .await??;
        let package_id = value
            .get("id")
            .and_then(serde_json::Value::as_str)
            .map(http::HeaderValue::from_str)
            .transpose()
            .map_err(|_| Error::InvalidConfig("invalid installed package ID".into()))?;
        let mut response = axum::Json(value).into_response();
        if let Some(id) = package_id {
            response.headers_mut().insert("package-id", id);
        }
        return Ok(response);
    }
    let runtime = sandbox.read().unwrap().runtime.clone();
    let value = tokio::task::spawn_blocking(move || {
        // The URL selects a local short name. Package identity remains the
        // signed publisher/name, including its registration and private data.
        let id = if checking {
            None
        } else if method == Method::PUT {
            let package = read_package(&bytes)?;
            let document = package_metadata(&package)?;
            let signed_id = document["id"].as_str().unwrap();
            if document["name"].as_str() != id.as_deref() {
                return Err(Error::BadRequest(
                    "package name does not match the management URL".into(),
                ));
            }
            Some(signed_id.to_owned())
        } else if let Some(name) = &id {
            use rusqlite::OptionalExtension;
            let path = profile.config_db_path();
            if !path.symlink_metadata()?.file_type().is_file() {
                return Err(Error::InvalidConfig(
                    "config.db must be a regular file".into(),
                ));
            }
            let conn = rusqlite::Connection::open_with_flags(
                path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )?;
            conn.busy_timeout(Duration::from_secs(3))?;
            let table = if app { "apps" } else { "libs" };
            let exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                [table],
                |row| row.get(0),
            )?;
            let registered: Option<String> = if exists {
                conn.query_row(
                    &format!("SELECT id FROM {table} WHERE name=?1"),
                    [name],
                    |row| row.get(0),
                )
                .optional()?
            } else {
                None
            };
            if let Some(expected) = &expected_id
                && registered.as_deref().map(str::as_bytes) != Some(expected.as_bytes())
            {
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "installed package identity changed; query the catalog before removing",
                )));
            }
            let Some(registered) = registered else {
                return if method == Method::DELETE {
                    if sandbox.read().unwrap().tasks.is_closed() {
                        Err(Error::Denied)
                    } else {
                        Ok(serde_json::Value::Null)
                    }
                } else {
                    Err(Error::RouteNotFound)
                };
            };
            valid_id(&registered)?;
            if registered.rsplit('/').next() != Some(name.as_str()) {
                return Err(Error::InvalidConfig(
                    "invalid installed package name".into(),
                ));
            }
            Some(registered)
        } else {
            None
        };
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
            unreachable!("App reads returned through the lightweight catalog");
        }
        if checking {
            check_lib(&bytes, &runtime)?;
            return Ok(lib_metadata(&read_package(&bytes)?, None));
        }
        match method {
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
