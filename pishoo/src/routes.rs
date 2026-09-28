use std::sync::Arc;

use access_control::AccessService;
use axum::{
    Router,
    body::Body as AxumBody,
    response::{IntoResponse, Response},
    routing::any,
};
use http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use tokio_util::io::ReaderStream;

use self::{
    access::{authorize, management_router},
    proxy::proxy,
};
use crate::{Error, Result, setup::ServerConfig};
mod access;

mod proxy;

fn reserved(path: &str) -> bool {
    [
        "/api",
        "/contact",
        "/contacts",
        "/acl",
        "/workspace",
        "/workspace-api",
        "/.pishoo",
        "/exec",
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

pub(crate) fn build_router(
    endpoint: dhttp::Endpoint,
    access: Arc<AccessService>,
    app_router: Router,
    config: &ServerConfig,
    profile: &dhttp_home::identity::IdentityProfile,
) -> Result<Router> {
    let proxies = config.proxy_locations.clone();
    let root = profile.join("file");
    let app = management_router(access.clone(), profile.name(), endpoint.name())
        .merge(app_router)
        .fallback(any(move |request: Request<AxumBody>| {
            let (endpoint, proxies, root) = (endpoint.clone(), proxies.clone(), root.clone());
            async move {
                let result: Result<Response> = async {
                    let path = request.uri().path();
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

#[cfg(test)]
#[path = "../tests/unit/routes.rs"]
mod tests;
