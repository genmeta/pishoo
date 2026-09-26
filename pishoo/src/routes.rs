use std::{collections::BTreeMap, sync::Arc};

use access_control::{AccessService, Action, AuthResult, Headers, SubjectId, Visitor};
use axum::{
    Router,
    body::Body as AxumBody,
    response::{IntoResponse, Response},
    routing::any,
};
use http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use tokio_util::{io::ReaderStream, task::TaskTracker};

use crate::{
    Body, Error, Result,
    setup::{ProxyLocation, ServerConfig},
    wasm::{Invocation, Lib},
};

pub(crate) fn build_router(
    endpoint: dhttp::Endpoint,
    access: Arc<AccessService>,
    libs: &BTreeMap<String, Arc<Lib>>,
    config: &ServerConfig,
    profile: &dhttp_home::identity::IdentityProfile,
    lib_slots: Arc<tokio::sync::Semaphore>,
    tasks: TaskTracker,
) -> Result<Router> {
    let libs = libs.clone();
    let proxies = config.proxy_locations.clone();
    let root = profile.join("file");
    let app = management_router(access.clone(), profile.name(), endpoint.name())
        .fallback(any(move |request: Request<AxumBody>| {
            let (endpoint, libs, proxies, root, slots, tasks) = (
                endpoint.clone(),
                libs.clone(),
                proxies.clone(),
                root.clone(),
                lib_slots.clone(),
                tasks.clone(),
            );
            async move {
                let result: Result<Response> = async {
                    let path = request.uri().path();
                    if path == "/api" || path.starts_with("/api/") {
                        let tail = path.strip_prefix("/api/").ok_or(Error::RouteNotFound)?;
                        let (id, suffix) = tail
                            .split_once('/')
                            .map_or((tail, "/".to_string()), |(id, p)| (id, format!("/{p}")));
                        let lib = libs.get(id).ok_or(Error::RouteNotFound)?.clone();
                        if lib.id != id {
                            return Err(Error::RouteNotFound);
                        }
                        if lib.cancel.is_cancelled() {
                            return Err(Error::Cancelled);
                        }
                        let item = lib
                            .openapi
                            .paths
                            .as_ref()
                            .and_then(|p| p.get(&suffix))
                            .ok_or(Error::RouteNotFound)?;
                        if !item
                            .methods()
                            .into_iter()
                            .any(|(method, _)| method == *request.method())
                        {
                            return Err(Error::MethodNotAllowed);
                        }
                        let permit = slots.try_acquire_owned().map_err(|_| Error::Capacity)?;
                        let handshake = request
                            .extensions()
                            .get::<dhttp::HandshakeSummary>()
                            .ok_or(Error::MissingHandshake)?;
                        let invocation = Invocation::new(lib, permit, endpoint, handshake, tasks)?;
                        let mut request = request.map(|b| b.map_err(Into::into).boxed_unsync());
                        let mut parts = request.uri().clone().into_parts();
                        let path = match request.uri().query() {
                            Some(q) => format!("{suffix}?{q}"),
                            None => suffix,
                        };
                        parts.path_and_query = Some(
                            path.parse()
                                .map_err(|_| Error::BadRequest("invalid API path".into()))?,
                        );
                        *request.uri_mut() = http::Uri::from_parts(parts)
                            .map_err(|_| Error::BadRequest("invalid request URI".into()))?;
                        return invocation
                            .execute(request)
                            .await
                            .map(|r| r.map(AxumBody::new));
                    }
                    if reserved(path) {
                        return Err(Error::RouteNotFound);
                    }
                    let exact = proxies
                        .iter()
                        .find(|p| p.location.strip_prefix("= ") == Some(path));
                    if exact.is_none() {
                        if let Some(route) = proxies.iter().find(|p| {
                            p.location.starts_with('/')
                                && p.location.ends_with('/')
                                && p.location.trim_end_matches('/') == path
                        }) {
                            let location = match request.uri().query() {
                                Some(q) => format!("{}?{q}", route.location),
                                None => route.location.clone(),
                            };
                            return Ok((
                                StatusCode::MOVED_PERMANENTLY,
                                [(header::LOCATION, location)],
                            )
                                .into_response());
                        }
                    }
                    let route = exact.or_else(|| {
                        proxies
                            .iter()
                            .filter(|p| {
                                p.location.starts_with('/') && path.starts_with(&p.location)
                            })
                            .max_by_key(|p| p.location.len())
                    });
                    if let Some(route) = route {
                        return proxy(
                            endpoint,
                            route.clone(),
                            request.map(|b| b.map_err(Into::into).boxed_unsync()),
                        )
                        .await
                        .map(|r| r.map(AxumBody::new));
                    }
                    static_file(&root, request).await
                }
                .await;
                result.unwrap_or_else(reject)
            }
        }))
        .layer(axum::middleware::from_fn_with_state(access, authorize));
    Ok(app)
}

pub(crate) async fn authorize(
    axum::extract::State(access): axum::extract::State<Arc<AccessService>>,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    request.extensions_mut().remove::<Visitor>();
    let Some(handshake) = request.extensions().get::<dhttp::HandshakeSummary>() else {
        return reject(Error::MissingHandshake);
    };
    if handshake.local.is_none() {
        return reject(Error::MissingHandshake);
    }
    let visitor = match &handshake.remote {
        Some(remote) => match dhttp::certificate::subject_id(remote.certificates())
            .ok()
            .and_then(|s| SubjectId::new(s).ok())
        {
            Some(subject) => Some(Visitor::new(remote.name(), subject)),
            None => return reject(Error::Denied),
        },
        None => None,
    };
    let headers = Headers {
        method: request.method().clone(),
        path: request
            .uri()
            .path_and_query()
            .map_or("/", |p| p.as_str())
            .to_string(),
        fields: request.headers().clone(),
        request_id: None,
    };
    let allowed = match access
        .auth(
            headers,
            visitor.as_ref().map(Visitor::name),
            visitor.as_ref().map(Visitor::subject_id),
        )
        .await
    {
        Ok(AuthResult::Allowed) => true,
        Ok(AuthResult::Denied) => false,
        Ok(AuthResult::Reviewing(id, state, registry)) => {
            let _cleanup = scopeguard::guard((state.clone(), registry), |(state, registry)| {
                state.cancel();
                registry.del(id);
            });
            matches!(state.await, Ok(Action::Allow))
        }
        Err(e) => {
            eprintln!("authorization failed: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    if !allowed {
        return reject(Error::Denied);
    }
    if let Some(visitor) = visitor {
        request.extensions_mut().insert(visitor);
    }
    next.run(request).await
}

pub(crate) fn management_router(
    access: Arc<AccessService>,
    profile: &str,
    owner_name: &str,
) -> Router {
    let context = serde_json::json!({ "profile": profile, "owner_name": owner_name, "development_identity": false, "demo_data": false, "version": env!("CARGO_PKG_VERSION") });
    access_control::management_router(access)
        .route(
            "/workspace-api/context",
            axum::routing::get(move || {
                let context = context.clone();
                async move { axum::Json(context) }
            }),
        )
        .route(
            "/workspace",
            any(|| async {
                (
                    StatusCode::TEMPORARY_REDIRECT,
                    [(header::LOCATION, "/workspace/")],
                )
            }),
        )
        .route("/workspace/", any(workspace))
        .route("/workspace/{*path}", any(workspace))
}

async fn workspace(request: Request<AxumBody>) -> Response {
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return reject(Error::MethodNotAllowed);
    }
    let path = request
        .uri()
        .path()
        .strip_prefix("/workspace/")
        .unwrap_or_default();
    if path.split('/').any(|p| p == "..") || std::path::Path::new(path).extension().is_some() {
        return reject(Error::RouteNotFound);
    }
    let html = include_str!("workspace.html");
    let mut response = (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        html,
    )
        .into_response();
    if request.method() == Method::HEAD {
        *response.body_mut() = AxumBody::empty();
    }
    response
}

async fn static_file(root: &std::path::Path, request: Request<AxumBody>) -> Result<Response> {
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return Err(Error::MethodNotAllowed);
    }
    let decoded = percent_encoding::percent_decode_str(request.uri().path())
        .decode_utf8()
        .map_err(|_| Error::BadRequest("invalid path encoding".into()))?;
    if decoded.contains(['\\', '\0']) || decoded.split('/').any(|p| p == ".." || p == ".") {
        return Err(Error::RouteNotFound);
    }
    let mut path = std::path::PathBuf::from(decoded.trim_start_matches('/'));
    let root = root.to_path_buf();
    let (file, metadata, path) = tokio::task::spawn_blocking(move || -> Result<_> {
        let dir = cap_std::fs::Dir::open_ambient_dir(root, cap_std::ambient_authority())
            .map_err(|_| Error::RouteNotFound)?;
        if path.as_os_str().is_empty() || dir.metadata(&path).is_ok_and(|m| m.is_dir()) {
            path.push("index.html");
        }
        let file = dir.open(&path).map_err(|_| Error::RouteNotFound)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(Error::RouteNotFound);
        }
        Ok((file.into_std(), metadata, path))
    })
    .await??;
    let body = if request.method() == Method::HEAD {
        AxumBody::empty()
    } else {
        AxumBody::from_stream(ReaderStream::with_capacity(
            tokio::fs::File::from_std(file),
            16 * 1024,
        ))
    };
    let mut response = Response::new(body);
    response
        .headers_mut()
        .insert(header::CONTENT_LENGTH, metadata.len().into());
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        mime_guess::from_path(path)
            .first_or_octet_stream()
            .as_ref()
            .parse()
            .unwrap(),
    );
    Ok(response)
}

pub(crate) async fn proxy(
    endpoint: dhttp::Endpoint,
    route: ProxyLocation,
    mut request: Request<Body>,
) -> Result<http::Response<Body>> {
    let original_host = request.uri().authority().map(|a| a.as_str().to_owned());
    let original_scheme = request.uri().scheme_str().unwrap_or("https").to_owned();
    *request.uri_mut() = proxy_uri(&route, request.uri())?;
    clean_hop_headers(request.headers_mut());
    let protocol = request.extensions_mut().remove::<Arc<str>>();
    request.extensions_mut().clear();
    if let Some(protocol) = protocol {
        request.extensions_mut().insert(protocol);
    }
    request.headers_mut().remove(header::HOST);
    let host = request
        .uri()
        .authority()
        .expect("validated proxy authority")
        .as_str()
        .parse()
        .map_err(|_| Error::BadRequest("invalid authority".into()))?;
    request.headers_mut().insert(header::HOST, host);
    for name in [
        "forwarded",
        "x-forwarded-for",
        "x-forwarded-host",
        "x-forwarded-proto",
    ] {
        request.headers_mut().remove(name);
    }
    if let Some(host) = original_host {
        request.headers_mut().insert(
            "x-forwarded-host",
            host.parse()
                .map_err(|_| Error::BadRequest("invalid forwarding authority".into()))?,
        );
    }
    request.headers_mut().insert(
        "x-forwarded-proto",
        original_scheme
            .parse()
            .map_err(|_| Error::BadRequest("invalid scheme".into()))?,
    );
    let mut response = endpoint.from_request(request).await?;
    clean_hop_headers(response.headers_mut());
    Ok(response)
}

fn proxy_uri(route: &ProxyLocation, uri: &http::Uri) -> Result<http::Uri> {
    let mut parts = http::uri::Parts::default();
    parts.scheme = route.proxy_pass.scheme.clone();
    parts.authority = route.proxy_pass.authority.clone();
    parts.path_and_query = route.proxy_pass.path_and_query.clone();
    let path = if let Some(base) = &parts.path_and_query {
        let suffix = if route.location.starts_with("= ") {
            ""
        } else {
            uri.path()
                .strip_prefix(&route.location)
                .ok_or(Error::RouteNotFound)?
        };
        format!("{}{suffix}", base.path())
    } else {
        uri.path().to_owned()
    };
    let path = match uri.query() {
        Some(q) => format!("{path}?{q}"),
        None => path,
    };
    parts.path_and_query = Some(
        path.parse()
            .map_err(|_| Error::BadRequest("invalid upstream path".into()))?,
    );
    http::Uri::from_parts(parts).map_err(|_| Error::BadRequest("invalid upstream URI".into()))
}
fn clean_hop_headers(headers: &mut http::HeaderMap) {
    let named = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|h| h.to_str().ok())
        .flat_map(|s| s.split(','))
        .filter_map(|s| s.trim().parse::<http::HeaderName>().ok())
        .collect::<Vec<_>>();
    for name in named {
        headers.remove(name);
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-connection",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.remove(name);
    }
    let reserved = headers
        .keys()
        .filter(|n| n.as_str().starts_with("pishoo-"))
        .cloned()
        .collect::<Vec<_>>();
    for name in reserved {
        headers.remove(name);
    }
}
fn reserved(path: &str) -> bool {
    [
        "/contact",
        "/contacts",
        "/acl",
        "/workspace",
        "/workspace-api",
        "/.pishoo",
        "/shell",
    ]
    .iter()
    .any(|p| path == *p || path.strip_prefix(p).is_some_and(|r| r.starts_with('/')))
}
fn reject(error: Error) -> Response {
    let status = error.status();
    if status.is_server_error() {
        eprintln!("request failed: {error}");
    }
    (
        status,
        status.canonical_reason().unwrap_or("request failed"),
    )
        .into_response()
}

#[cfg(test)]
mod tests;
