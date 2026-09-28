use std::path::PathBuf;

use axum::{
    Router,
    body::Body as AxumBody,
    response::{IntoResponse, Response},
    routing::any,
};
use http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use tokio_util::io::ReaderStream;

use self::proxy::proxy;
use crate::{Error, Result, setup::ProxyLocation};
mod access;

mod proxy;

pub(crate) use access::{authorize, worksapce};

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

pub(crate) fn file_router(root: PathBuf) -> Router {
    Router::new()
        .route("/file", any(|| async { StatusCode::NOT_FOUND }))
        .route(
            "/file/{*path}",
            any(move |request: Request<AxumBody>| {
                let root = root.clone();
                async move { static_file(&root, request).await.unwrap_or_else(reject) }
            }),
        )
}

pub(crate) async fn proxy_pass(
    proxies: Vec<ProxyLocation>,
    request: Request<AxumBody>,
) -> Response {
    let result: Result<Response> = async {
        let path = request.uri().path();
        if reserved(path) || path == "/file" || path.starts_with("/file/") {
            return Err(Error::RouteNotFound);
        }
        let exact = proxies
            .iter()
            .find(|p| p.location.strip_prefix("= ") == Some(path));
        if exact.is_none()
            && let Some(route) = proxies
                .iter()
                .find(|p| p.location.ends_with('/') && p.location.trim_end_matches('/') == path)
        {
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
        let route = exact.or_else(|| {
            proxies
                .iter()
                .filter(|p| {
                    p.location.starts_with('/')
                        && (p.location == "/"
                            || path == p.location.trim_end_matches('/')
                            || path.starts_with(&format!("{}/", p.location.trim_end_matches('/'))))
                })
                .max_by_key(|p| p.location.len())
        });
        let route = route.ok_or(Error::RouteNotFound)?;
        proxy(
            route.clone(),
            request.map(|b| b.map_err(Into::into).boxed_unsync()),
        )
        .await
        .map(|r| r.map(AxumBody::new))
    }
    .await;
    match result {
        Ok(response) => response,
        Err(Error::Io(error)) => {
            eprintln!("local proxy upstream failed: {error}");
            StatusCode::BAD_GATEWAY.into_response()
        }
        Err(error) => reject(error),
    }
}

async fn static_file(root: &std::path::Path, request: Request<AxumBody>) -> Result<Response> {
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return Err(Error::MethodNotAllowed);
    }
    let path = request
        .uri()
        .path()
        .strip_prefix("/file/")
        .ok_or(Error::RouteNotFound)?;
    if path.is_empty() {
        return Err(Error::RouteNotFound);
    }
    let decoded = percent_encoding::percent_decode_str(path)
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
