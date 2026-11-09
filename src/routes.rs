use axum::{
    body::Body as AxumBody,
    response::{IntoResponse, Response},
};
use http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;

use self::proxy::proxy;
use crate::{Error, Result, setup::ProxyLocation};
mod access;
mod capabilities;
mod outbound;

mod proxy;

pub(crate) const DHTTP_PREFIX: &str = "/std/forward/";

pub(crate) use access::{access_router, authorize};
pub(crate) use capabilities::{capabilities_router, permissions};
pub(crate) use outbound::forward_dhttp;

pub(crate) fn reserved(path: &str) -> bool {
    path == "/std" || path.starts_with("/std/")
}

/// Normalize before ACL and routing, so authorization observes the effective method.
pub(crate) async fn normalize_method(
    mut request: Request<AxumBody>,
    next: axum::middleware::Next,
) -> Response {
    let mut values = request.headers().get_all("x-http-method-override").iter();
    if let Some(value) = values.next() {
        let method = Method::from_bytes(value.as_bytes());
        if values.next().is_some()
            || request.method() != Method::POST
            || !method.as_ref().is_ok_and(|method| {
                matches!(
                    *method,
                    Method::GET
                        | Method::HEAD
                        | Method::POST
                        | Method::PUT
                        | Method::PATCH
                        | Method::DELETE
                        | Method::OPTIONS
                )
            })
        {
            return (
                StatusCode::BAD_REQUEST,
                [(header::CACHE_CONTROL, "no-store")],
                "invalid X-HTTP-Method-Override",
            )
                .into_response();
        }
        *request.method_mut() = method.unwrap();
        request.headers_mut().remove("x-http-method-override");
    }
    next.run(request).await
}

fn reject(error: Error) -> Response {
    let status = error.status();
    if status.is_server_error() {
        tracing::error!(%error, "request failed");
    }
    (
        status,
        status.canonical_reason().unwrap_or("request failed"),
    )
        .into_response()
}

pub(crate) async fn proxy_pass(
    proxies: Vec<ProxyLocation>,
    request: Request<AxumBody>,
) -> Response {
    let result: Result<Response> = async {
        let path = request.uri().path();
        if reserved(path) {
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
            tracing::warn!(%error, "local proxy upstream failed");
            StatusCode::BAD_GATEWAY.into_response()
        }
        Err(error) => reject(error),
    }
}

#[cfg(test)]
#[path = "../tests/unit/routes/tests.rs"]
pub(crate) mod tests;
