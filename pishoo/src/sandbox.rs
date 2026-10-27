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

mod host;
mod runtime;

use crate::package::{app_file, disk_bytes, installed_apps, lib_root};

pub(crate) struct Sandbox {
    pub(crate) libs: HashMap<String, Arc<Lib>>,
    pub(crate) apps: HashMap<String, (String, Arc<std::fs::File>)>,
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
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "runtime snapshot retained by the frozen v1 contract"
        )
    )]
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
        let database = profile.join(".volume/sys/setting/database");
        if !database.symlink_metadata()?.file_type().is_file() {
            return Err(Error::InvalidConfig(
                "config.db must be a regular file".into(),
            ));
        }
        let conn = crate::setup::open_config(profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.busy_timeout(Duration::from_secs(3))?;
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_list WHERE type IN ('table','view') AND name='lib')",
            [],
            |row| row.get(0),
        )?;
        let registered = if exists {
            let author: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('lib') WHERE name='author')",
                [],
                |row| row.get(0),
            )?;
            conn.prepare(if author {
                "SELECT CASE WHEN author IS NULL THEN name ELSE author||'/'||name END FROM lib ORDER BY author,name"
            } else {
                "SELECT id FROM lib ORDER BY id"
            })?
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
                    let bytes = disk_bytes(profile, &root, id)?;
                    let lib = Lib::load(
                        self.runtime.clone(),
                        id.clone(),
                        &bytes,
                        &profile
                            .join(".volume")
                            .join(if id.starts_with("std.dhttp.net/") {
                                format!("std/{}", id.rsplit('/').next().unwrap())
                            } else {
                                id.rsplit('/').next().unwrap().to_owned()
                            }),
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
        use tower::ServiceExt;

        let frontend_sandbox = sandbox.clone();
        let api = axum::routing::any(move |request: http::Request<axum::body::Body>| {
            let (endpoint, sandbox, access, native_api) = (
                endpoint.clone(),
                sandbox.clone(),
                access.clone(),
                native_api.clone(),
            );
            async move {
                let result: Result<axum::response::Response> = async {
                    // One exact snapshot, then release the shared collection lock.
                    let (lib, path, tasks) = {
                        let sandbox = sandbox.read().unwrap();
                        let path = request.uri().path();
                        let (standard, tail) = match path.strip_prefix("/std/") {
                            Some(tail) => (true, tail),
                            None => (false, path.strip_prefix('/').unwrap_or(path)),
                        };
                        let name = tail.split('/').next().unwrap_or("");
                        let lib = sandbox
                            .libs
                            .iter()
                            .find(|(id, _)| {
                                id.starts_with("std.dhttp.net/") == standard
                                    && id.rsplit('/').next() == Some(name)
                            })
                            .map(|(_, lib)| lib.clone())
                            .ok_or(Error::RouteNotFound)?;
                        // Directory placement selects the Lib, never OpenAPI.
                        // Standard guests keep /<name>/... after removing /std;
                        // ordinary guests receive the suffix after /<name>.
                        let path = if standard {
                            format!("/{tail}")
                        } else {
                            match tail.strip_prefix(name).unwrap() {
                                "" => "/".to_owned(),
                                suffix => suffix.to_owned(),
                            }
                        };
                        (lib, path, sandbox.tasks.clone())
                    };
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
        let lib_api = api.clone();
        let frontend = axum::routing::any(move |request: http::Request<axum::body::Body>| {
            let sandbox = frontend_sandbox.clone();
            let api = lib_api.clone();
            async move {
                if !request.uri().path().starts_with("/app/") {
                    return api.oneshot(request).await.expect("route is infallible");
                }
                let result =
                    tokio::task::spawn_blocking(move || -> Result<axum::response::Response> {
                        use axum::response::IntoResponse;
                        use http::{Method, StatusCode, header};
                        let tail = request
                            .uri()
                            .path()
                            .strip_prefix("/app/")
                            .ok_or(Error::RouteNotFound)?;
                        let name = tail.split('/').next().unwrap();
                        // Keep installation/uninstallation from removing the selected directory
                        // until the requested file is open. The response owns only that file.
                        let sandbox = sandbox.read().unwrap();
                        let (_, directory) = sandbox
                            .apps
                            .iter()
                            .find(|(id, _)| id.rsplit('/').next() == Some(name))
                            .map(|(_, app)| app)
                            .ok_or(Error::RouteNotFound)?;
                        if !matches!(request.method(), &Method::GET | &Method::HEAD) {
                            return Ok((
                                StatusCode::METHOD_NOT_ALLOWED,
                                [(header::ALLOW, "GET, HEAD")],
                            )
                                .into_response());
                        }
                        let suffix = tail.strip_prefix(name).unwrap();
                        if suffix.is_empty() {
                            let location = match request.uri().query() {
                                Some(query) => format!("{}/?{query}", request.uri().path()),
                                None => format!("{}/", request.uri().path()),
                            };
                            return Ok(
                                axum::response::Redirect::temporary(&location).into_response()
                            );
                        }
                        let suffix = percent_encoding::percent_decode_str(suffix)
                            .decode_utf8()
                            .map_err(|_| Error::RouteNotFound)?;
                        if suffix.contains(['\\', '\0', '%'])
                            || suffix.split('/').any(|part| matches!(part, "." | ".."))
                            || suffix.chars().any(|c| c.is_control())
                        {
                            return Err(Error::RouteNotFound);
                        }
                        let mut file = if suffix == "/" {
                            "index.html".to_owned()
                        } else {
                            suffix.trim_start_matches('/').to_owned()
                        };
                        let opened = match app_file(directory, &file) {
                            Ok(file) => file,
                            Err(_) => {
                                let resource = suffix
                                    .trim_end_matches('/')
                                    .rsplit('/')
                                    .next()
                                    .unwrap_or("");
                                let asset = suffix.starts_with("/assets/")
                                    || suffix.starts_with("/static/")
                                    || matches!(suffix.as_ref(), "/assets" | "/static")
                                    || resource.contains('.')
                                    || request
                                        .headers()
                                        .get("sec-fetch-dest")
                                        .and_then(|value| value.to_str().ok())
                                        .is_some_and(|value| {
                                            !matches!(value, "document" | "iframe" | "empty")
                                        });
                                if asset {
                                    return Err(Error::RouteNotFound);
                                }
                                file = "index.html".to_owned();
                                app_file(directory, &file)?
                            }
                        };
                        let length = opened.metadata()?.len();
                        drop(sandbox);
                        http::Response::builder()
                            .status(StatusCode::OK)
                            .header(
                                header::CONTENT_TYPE,
                                mime_guess::from_path(&file)
                                    .first_or_octet_stream()
                                    .to_string(),
                            )
                            .header(header::CONTENT_LENGTH, length)
                            .header(header::CACHE_CONTROL, "no-store")
                            .header("x-content-type-options", "nosniff")
                            .header("referrer-policy", "no-referrer")
                            .header("cross-origin-resource-policy", "same-origin")
                            .header("x-frame-options", "DENY")
                            .body(if request.method() == Method::HEAD {
                                axum::body::Body::empty()
                            } else {
                                axum::body::Body::from_stream(
                                    tokio_util::io::ReaderStream::with_capacity(
                                        tokio::fs::File::from_std(opened),
                                        16 * 1024,
                                    ),
                                )
                            })
                            .map_err(Error::Http)
                    })
                    .await;
                result
                    .map_err(Error::from)
                    .and_then(|response| response)
                    .unwrap_or_else(reject_api)
            }
        });
        axum::Router::new()
            .route(
                "/std",
                axum::routing::any(|| async { reject_api(Error::RouteNotFound) }),
            )
            .route("/std/{*path}", api)
            .fallback(frontend)
    }
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

#[cfg(test)]
#[path = "../tests/unit/sandbox.rs"]
pub(crate) mod tests;
