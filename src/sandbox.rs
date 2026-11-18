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

use crate::package::{app_file, installed_apps, lib_root};

pub(crate) struct Sandbox {
    pub(crate) libs: HashMap<String, Arc<Lib>>,
    pub(crate) versions: HashMap<(String, String), Arc<Lib>>,
    pub(crate) apps: HashMap<String, (String, Arc<std::fs::File>)>,
    pub(crate) runtime: Arc<WasmRuntime>,
    pub(crate) tasks: TaskTracker,
    pub(crate) dev_libs: HashMap<String, (std::fs::Metadata, Arc<Lib>)>,
}

pub struct WasmRuntime {
    engine: Engine,
    linker: Linker<StoreData>,
    _epoch_shutdown: std::sync::mpsc::Sender<()>,
}

pub(crate) struct Lib {
    pub(crate) id: String,
    pub(crate) author_owner_hash: Option<String>,
    pub(crate) dependencies: HashMap<String, (String, String)>,
    commands: Arc<HashMap<String, Component>>,
    pub(crate) stream_retired: tokio_util::sync::CancellationToken,
    pub(crate) openapi: oas3::OpenApiV3Spec,
    component: Component,
    runtime: Arc<WasmRuntime>,
    filesystem: WasiFilesystemCtx,
}

struct StoreData {
    caller_service: Option<(String, String, String)>,
    commands: Arc<HashMap<String, Component>>,
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
        source: Option<Arc<Lib>>,
        endpoint: dhttp::Endpoint,
        access: Arc<access_control::AccessService>,
        api: axum::Router,
    },
}

impl Sandbox {
    pub(crate) fn new(runtime: Arc<WasmRuntime>) -> Self {
        Self {
            libs: HashMap::new(),
            versions: HashMap::new(),
            apps: HashMap::new(),
            runtime,
            tasks: TaskTracker::new(),
            dev_libs: HashMap::new(),
        }
    }

    /// Close WASM task tracking and discard the published Lib set.
    /// Server separately clears the HTTP router.
    pub(crate) fn close(&mut self) {
        for lib in self
            .versions
            .values()
            .chain(self.libs.values())
            .chain(self.dev_libs.values().map(|(_, lib)| lib))
        {
            lib.stream_retired.cancel();
        }
        self.tasks.close();
        self.libs.clear();
        self.versions.clear();
        self.apps.clear();
        self.dev_libs.clear();
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
        crate::package::migrate_legacy_packages(profile)?;
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
        let active = crate::package::installed_packages(profile, None)?;
        let active_versions = active
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                (
                    entry["id"].as_str().unwrap(),
                    entry["version"].as_str().unwrap(),
                )
            })
            .collect::<HashMap<_, _>>();
        let metadata = crate::package::installed_package_versions(profile, None)?;
        let mut pending = metadata
            .into_iter()
            .filter(|entry| entry["has_lib"] == true)
            .collect::<Vec<_>>();
        let mut versions: HashMap<(String, String), Arc<Lib>> = HashMap::new();
        let root = if pending.is_empty() {
            None
        } else {
            Some(lib_root(profile)?)
        };
        while !pending.is_empty() {
            let entry = pending.pop().unwrap();
            let id = entry["id"]
                .as_str()
                .ok_or_else(|| Error::InvalidConfig("missing installed identity".into()))?;
            let version = entry["version"]
                .as_str()
                .ok_or_else(|| Error::InvalidConfig("missing installed version".into()))?;
            let bytes = if active_versions
                .get(id)
                .is_some_and(|active| *active == version)
            {
                crate::package::disk_bytes(profile, root.as_ref().unwrap(), id)?
            } else {
                crate::package::disk_version_bytes(
                    profile,
                    root.as_ref().unwrap(),
                    id,
                    Some(version),
                )?
            };
            let mut lib = Lib::load(
                self.runtime.clone(),
                id.to_owned(),
                &bytes,
                &profile.join(".volume").join(
                    if crate::package::is_standard_author(id.split('/').next().unwrap_or(""))
                        && !package_name(id).starts_with("std/")
                    {
                        format!("std/{}", package_name(id))
                    } else {
                        package_name(id).to_owned()
                    },
                ),
            )?;
            lib.author_owner_hash = entry["author_owner_hash"].as_str().map(str::to_owned);
            if lib.openapi.info.version != version {
                return Err(Error::InvalidComponent(format!(
                    "installed {id} {version} has runtime version {}",
                    lib.openapi.info.version
                )));
            }
            for dependency in entry["dependencies"].as_array().unwrap() {
                let name = dependency["name"].as_str().unwrap();
                let id = dependency["publisher"]
                    .as_str()
                    .map_or_else(|| name.to_owned(), |author| format!("{author}/{name}"));
                lib.dependencies.insert(
                    name.to_owned(),
                    (id, dependency["version"].as_str().unwrap().to_owned()),
                );
            }
            versions.insert((id.to_owned(), version.to_owned()), Arc::new(lib));
        }
        let mut candidates = HashMap::new();
        for entry in active.as_array().unwrap() {
            if entry["has_lib"] == true {
                let id = entry["id"].as_str().unwrap();
                let version = entry["version"].as_str().unwrap();
                let lib = versions
                    .get(&(id.to_owned(), version.to_owned()))
                    .ok_or_else(|| Error::InvalidConfig("missing active version".into()))?;
                candidates.insert(id.to_owned(), Arc::clone(lib));
            }
        }
        let apps = installed_apps(profile, None)?;
        for lib in self.versions.values().chain(self.libs.values()) {
            lib.stream_retired.cancel();
        }
        self.versions = versions;
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
                    let (lib, path, tasks) = {
                        let sandbox = sandbox.read().unwrap();
                        let (lib, path) = select_library(&sandbox, &endpoint, &request)?;
                        (lib, path, sandbox.tasks.clone())
                    };
                    let version = lib.openapi.info.version.clone();
                    if request.uri().path().starts_with("/std/") {
                        let methods = lib
                            .openapi
                            .paths
                            .as_ref()
                            .unwrap()
                            .iter()
                            .find(|(pattern, _)| crate::package::api_path_matches(pattern, &path))
                            .ok_or(Error::RouteNotFound)?
                            .1
                            .methods()
                            .into_iter()
                            .map(|(method, _)| method.as_str().to_owned())
                            .collect::<Vec<_>>();
                        if !methods
                            .iter()
                            .any(|method| method == request.method().as_str())
                        {
                            use axum::response::IntoResponse;
                            return Ok(version_response(
                                (
                                    http::StatusCode::METHOD_NOT_ALLOWED,
                                    [(http::header::ALLOW, methods.join(", "))],
                                )
                                    .into_response(),
                                &version,
                            ));
                        }
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
                            source: None,
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
                    invocation.execute(request).await.map(|response| {
                        version_response(response.map(axum::body::Body::new), &version)
                    })
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
                        let tail = request
                            .uri()
                            .path()
                            .strip_prefix("/app/")
                            .ok_or(Error::RouteNotFound)?;
                        // Select the complete registered name at a path-segment boundary.
                        let sandbox = sandbox.read().unwrap();
                        let (name, directory) = sandbox
                            .apps
                            .iter()
                            .filter_map(|(id, (_, directory))| {
                                let name = package_name(id);
                                tail.strip_prefix(name)
                                    .filter(|suffix| suffix.is_empty() || suffix.starts_with('/'))
                                    .map(|_| (name, directory))
                            })
                            .max_by_key(|(name, _)| name.len())
                            .ok_or(Error::RouteNotFound)?;
                        app_response(directory, tail.strip_prefix(name).unwrap(), &request)
                    })
                    .await;
                result
                    .map_err(Error::from)
                    .and_then(|response| response)
                    .unwrap_or_else(|error| {
                        use axum::response::IntoResponse;
                        (
                            [(http::header::CACHE_CONTROL, "no-store")],
                            reject_api(error),
                        )
                            .into_response()
                    })
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

pub(crate) fn requested_version(request: &http::Request<axum::body::Body>) -> Result<Option<&str>> {
    let mut values = request.headers().get_all("pishoo-package-version").iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    let version = value.to_str().map_err(|_| Error::PackagePolicy {
        code: "invalid_package_version",
        status: http::StatusCode::BAD_REQUEST,
    })?;
    if values.next().is_some()
        || version.split('.').count() != 3
        || version.split('.').any(|part| {
            part.is_empty()
                || !part.bytes().all(|byte| byte.is_ascii_digit())
                || part.parse::<u64>().is_err()
                || part.len() > 1 && part.starts_with('0')
        })
    {
        return Err(Error::PackagePolicy {
            code: "invalid_package_version",
            status: http::StatusCode::BAD_REQUEST,
        });
    }
    Ok(Some(version))
}

pub(crate) fn select_library(
    sandbox: &Sandbox,
    endpoint: &dhttp::Endpoint,
    request: &http::Request<axum::body::Body>,
) -> Result<(Arc<Lib>, String)> {
    let requested = requested_version(request)?;
    let path = request.uri().path();
    let standard = path.starts_with("/std/");
    let standard_path = path.strip_prefix("/std").unwrap_or(path);
    let tail = path.trim_start_matches('/');
    let source = request.extensions().get::<Arc<Lib>>();
    let candidates = sandbox
        .versions
        .iter()
        .map(|((id, _), lib)| (id.as_str(), lib))
        .chain(sandbox.libs.iter().map(|(id, lib)| (id.as_str(), lib)))
        .chain(
            sandbox
                .dev_libs
                .iter()
                .map(|(id, (_, lib))| (id.as_str(), lib)),
        )
        .chain(source.into_iter().map(|lib| (lib.id.as_str(), lib)));
    let mut providers = std::collections::BTreeSet::new();
    for (id, lib) in candidates {
        let matches = if standard {
            crate::package::is_standard_author(id.split('/').next().unwrap_or(""))
                && lib.openapi.paths.as_ref().is_some_and(|paths| {
                    paths
                        .keys()
                        .any(|pattern| crate::package::api_path_matches(pattern, standard_path))
                })
        } else {
            !crate::package::is_standard_author(id.split('/').next().unwrap_or(""))
                && tail
                    .strip_prefix(package_name(id))
                    .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with('/'))
        };
        if matches {
            providers.insert(id.to_owned());
        }
    }
    let id = if standard {
        if providers.len() > 1 {
            return Err(Error::PackagePolicy {
                code: "route_conflict",
                status: http::StatusCode::CONFLICT,
            });
        }
        providers.into_iter().next()
    } else {
        providers
            .into_iter()
            .max_by_key(|id| package_name(id).len())
    }
    .ok_or(Error::RouteNotFound)?;
    let bound = source.and_then(|source| {
        if source.id == id {
            Some(Arc::clone(source))
        } else {
            source.dependencies.get(package_name(&id)).and_then(|key| {
                sandbox
                    .versions
                    .get(key)
                    .or_else(|| {
                        sandbox
                            .libs
                            .get(&key.0)
                            .filter(|lib| lib.openapi.info.version == key.1)
                    })
                    .or_else(|| {
                        sandbox
                            .dev_libs
                            .get(&key.0)
                            .map(|(_, lib)| lib)
                            .filter(|lib| lib.openapi.info.version == key.1)
                    })
                    .filter(|lib| lib.id == id)
                    .cloned()
            })
        }
    });
    if bound.is_none()
        && source.is_some_and(|source| source.dependencies.contains_key(package_name(&id)))
    {
        return Err(Error::PackagePolicy {
            code: "package_version_unavailable",
            status: http::StatusCode::NOT_ACCEPTABLE,
        });
    }
    if let Some(bound) = &bound
        && requested.is_some_and(|version| version != bound.openapi.info.version)
    {
        return Err(Error::PackagePolicy {
            code: "dependency_version_conflict",
            status: http::StatusCode::CONFLICT,
        });
    }
    let owner = crate::package::require_owner(endpoint, request).is_ok();
    let selected_version = bound
        .as_ref()
        .map(|lib| lib.openapi.info.version.as_str())
        .or(requested);
    let preview = sandbox.dev_libs.get(&id).map(|(_, lib)| lib).filter(|lib| {
        owner && selected_version.is_none_or(|version| lib.openapi.info.version == version)
    });
    let lib = if let Some(preview) = preview {
        Arc::clone(preview)
    } else if let Some(bound) = bound {
        bound
    } else if let Some(version) = requested {
        sandbox
            .versions
            .get(&(id.clone(), version.to_owned()))
            .or_else(|| {
                sandbox
                    .libs
                    .get(&id)
                    .filter(|lib| lib.openapi.info.version == version)
            })
            .cloned()
            .ok_or(Error::PackagePolicy {
                code: "package_version_unavailable",
                status: http::StatusCode::NOT_ACCEPTABLE,
            })?
    } else {
        sandbox.libs.get(&id).cloned().ok_or_else(|| {
            if !owner && sandbox.dev_libs.contains_key(&id) {
                Error::Denied
            } else {
                Error::PackagePolicy {
                    code: "package_inactive",
                    status: http::StatusCode::SERVICE_UNAVAILABLE,
                }
            }
        })?
    };
    let path = if standard {
        if !lib.openapi.paths.as_ref().is_some_and(|paths| {
            paths
                .keys()
                .any(|pattern| crate::package::api_path_matches(pattern, standard_path))
        }) {
            return Err(Error::RouteNotFound);
        }
        standard_path.to_owned()
    } else {
        match tail
            .strip_prefix(package_name(&id))
            .ok_or(Error::RouteNotFound)?
        {
            "" => "/".to_owned(),
            suffix => suffix.to_owned(),
        }
    };
    Ok((lib, path))
}

pub(crate) fn version_response(
    mut response: axum::response::Response,
    version: &str,
) -> axum::response::Response {
    if let Ok(value) = http::HeaderValue::from_str(version) {
        response
            .headers_mut()
            .insert("pishoo-package-version", value);
    }
    let varies = response
        .headers()
        .get_all(http::header::VARY)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|value| {
            value.split(',').any(|name| {
                name.trim().eq_ignore_ascii_case("pishoo-package-version") || name.trim() == "*"
            })
        });
    if !varies {
        response.headers_mut().append(
            http::header::VARY,
            http::HeaderValue::from_static("Pishoo-Package-Version"),
        );
    }
    response
}

pub(crate) fn app_response(
    directory: &std::fs::File,
    suffix: &str,
    request: &http::Request<axum::body::Body>,
) -> Result<axum::response::Response> {
    use axum::response::IntoResponse;
    use http::{Method, StatusCode, header};
    if !matches!(request.method(), &Method::GET | &Method::HEAD) {
        return Ok((
            StatusCode::METHOD_NOT_ALLOWED,
            [
                (header::ALLOW, "GET, HEAD"),
                (header::CACHE_CONTROL, "no-store"),
            ],
        )
            .into_response());
    }
    if suffix.is_empty() {
        let location = match request.uri().query() {
            Some(query) => format!("{}/?{query}", request.uri().path()),
            None => format!("{}/", request.uri().path()),
        };
        return Ok((
            [(header::CACHE_CONTROL, "no-store")],
            axum::response::Redirect::temporary(&location),
        )
            .into_response());
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
                || mime_guess::from_path(resource).first().is_some()
                || request
                    .headers()
                    .get("sec-fetch-dest")
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(|value| !matches!(value, "document" | "iframe" | "empty"));
            if asset {
                return Err(Error::RouteNotFound);
            }
            file = "index.html".to_owned();
            app_file(directory, &file)?
        }
    };
    let mut response = crate::static_files::response(opened, &file, request)?;
    for (name, value) in [
        ("referrer-policy", "no-referrer"),
        ("cross-origin-resource-policy", "same-origin"),
        ("x-frame-options", "DENY"),
    ] {
        response
            .headers_mut()
            .insert(name, http::HeaderValue::from_static(value));
    }
    Ok(response)
}

fn reject_api(error: Error) -> axum::response::Response {
    if let Error::PackagePolicy { code, status } = &error {
        return axum::response::IntoResponse::into_response((
            *status,
            axum::Json(serde_json::json!({"error":{"code":code}})),
        ));
    }
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
#[path = "../tests/unit/sandbox/tests.rs"]
pub(crate) mod tests;

fn package_name(id: &str) -> &str {
    id.split_once('/')
        .filter(|(author, _)| dhttp_home::normalize_name(author).as_deref() == Some(*author))
        .map_or(id, |(_, name)| name)
}
