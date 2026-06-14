use std::{str::FromStr, sync::Arc};

use axum::{Extension, response::IntoResponse};
use http::{Request, Response, StatusCode, Uri, Version};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use snafu::{Report, ResultExt};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};
use tracing::{Instrument, debug, error};

use crate::{
    command,
    error::{Result, Whatever},
    parse::{pattern::Pattern, types::ProxyPass},
    reverse::location::LocationMatch,
};

/// Axum-style handler for reverse proxy requests.
///
/// Receives the request with body from the client, forwards it to the
/// configured upstream, and returns the upstream's response (optionally
/// gzip-compressed).
pub async fn proxy_handle(
    Extension(loc): Extension<LocationMatch>,
    req: Request<axum::body::Body>,
) -> impl IntoResponse {
    match proxy_inner(&loc, req).await {
        Ok(response) => response,
        Err(error) => {
            error!(error = %Report::from_error(&error), "proxy request failed");
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
}

async fn proxy_inner(
    loc: &LocationMatch,
    req: Request<axum::body::Body>,
) -> Result<axum::response::Response> {
    let location = &loc.location;

    // proxy_set_header
    let req = command::proxy_set_header(location, req);

    let accept_encoding = req
        .headers()
        .get(http::header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned());
    let public_origin = super::request_uri::request_public_origin(&req);

    debug!(headers = ?req.headers(), "processing request headers");

    let resp = pass(loc, req).await?;

    debug!("sending response");
    let (mut parts, body) = resp.into_parts();

    // add custom response headers
    command::add_header(location, &mut parts);
    if let Ok(Some(proxy_pass)) = location.get::<ProxyPass>("proxy_pass")
        && let Some(public_base) = public_base_for_proxy_redirect(loc, &proxy_pass)
        && let Some(public_origin) = public_origin.as_deref()
    {
        rewrite_proxy_redirect_default(&proxy_pass, public_origin, public_base, &mut parts.headers);
    }

    let resp = http::Response::from_parts(parts, body);
    Ok(super::gzip::compress_response(
        location,
        accept_encoding.as_deref(),
        resp,
    ))
}

/// Forward the request to the configured upstream.
fn pass(
    loc: &LocationMatch,
    req: Request<axum::body::Body>,
) -> impl std::future::Future<Output = Result<Response<Incoming>>> + Send {
    let location = Arc::clone(&loc.location);
    let loc = loc.clone();
    async move {
        let (parts, body) = req.into_parts();
        let proxy_pass = location
            .require::<ProxyPass>("proxy_pass")
            .whatever_context::<_, Whatever>("failed to read proxy_pass directive")?;

        tracing::debug!(proxy_pass = %proxy_pass.raw, "resolved proxy_pass target");

        let normalized = super::request_uri::normalize_request_uri(&parts.uri)
            .whatever_context::<_, Whatever>("failed to normalize request uri")?;
        let target =
            super::request_uri::build_upstream_request_target(&proxy_pass, &loc, &normalized)
                .whatever_context::<_, Whatever>("failed to build proxy upstream request target")?;

        tracing::info!(target, "proxying request to upstream path and query");

        let target_uri = Uri::from_str(&target).whatever_context::<_, Whatever>(format!(
            "failed to generate target uri from `{target}`"
        ))?;

        let mut new_parts = parts;
        new_parts.uri = target_uri.clone();
        new_parts.version = Version::HTTP_11;

        debug!(request = ?new_parts, "preparing upstream request");

        let scheme = proxy_pass.scheme_str();
        let host = proxy_pass.host();
        let port = proxy_pass.port_u16().unwrap_or(match scheme {
            "http" => 80,
            "https" => 443,
            _ => unreachable!("unsupported proxy_pass scheme"),
        });

        match scheme {
            "http" => {
                let io = TcpStream::connect((host, port))
                    .await
                    .whatever_context::<_, Whatever>(format!(
                        "cannot connect to target server {host}:{port}"
                    ))?;
                send_request(io, new_parts, body, target_uri).await
            }
            "https" => {
                let io = super::upstream_tls::connect_https(&location, &proxy_pass.uri).await?;
                send_request(io, new_parts, body, target_uri).await
            }
            _ => unreachable!("unsupported proxy_pass scheme"),
        }
    }
}

async fn send_request<I>(
    io: I,
    new_parts: http::request::Parts,
    body: axum::body::Body,
    target_uri: Uri,
) -> Result<Response<Incoming>>
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let io = TokioIo::new(io);

    let (mut sender, conn) = hyper::client::conn::http1::Builder::new()
        .preserve_header_case(true)
        .title_case_headers(true)
        .handshake(io)
        .await
        .whatever_context::<_, Whatever>("failed to establish http/1.1 client connection")?;

    debug!(%target_uri, "http client connection established");

    tokio::spawn(
        async move {
            if let Err(error) = conn.await {
                error!(
                    error = %Report::from_error(error),
                    "connection maintenance failed"
                );
            }
        }
        .in_current_span(),
    );

    let response = sender
        .send_request(Request::from_parts(new_parts, body))
        .await
        .whatever_context::<_, Whatever>("failed to send request to target")?;

    debug!("finished sending request body");
    Ok(response)
}

fn public_base_for_proxy_redirect<'a>(
    loc: &'a LocationMatch,
    proxy_pass: &ProxyPass,
) -> Option<&'a str> {
    match loc.pattern() {
        Pattern::Exact(_) | Pattern::Prefix(_) | Pattern::NormalPrefix(_) | Pattern::Common => {
            Some(loc.matched.as_str())
        }
        Pattern::Regex(_) | Pattern::CRegex(_) if !proxy_pass.has_explicit_uri() => Some("/"),
        Pattern::Regex(_) | Pattern::CRegex(_) => None,
    }
}

fn rewrite_proxy_redirect_default(
    proxy_pass: &ProxyPass,
    public_origin: &str,
    public_base: &str,
    headers: &mut http::HeaderMap,
) {
    rewrite_location_header(proxy_pass, public_origin, public_base, headers);
    rewrite_refresh_header(proxy_pass, public_origin, public_base, headers);
}

fn rewrite_location_header(
    proxy_pass: &ProxyPass,
    public_origin: &str,
    public_base: &str,
    headers: &mut http::HeaderMap,
) {
    let Some(value) = headers.get(http::header::LOCATION).cloned() else {
        return;
    };
    let Ok(text) = value.to_str() else {
        return;
    };
    let Some(rewritten) =
        rewrite_absolute_upstream_url(proxy_pass, public_origin, public_base, text)
    else {
        return;
    };

    headers.insert(
        http::header::LOCATION,
        rewritten.parse().expect("valid location value"),
    );
}

fn rewrite_refresh_header(
    proxy_pass: &ProxyPass,
    public_origin: &str,
    public_base: &str,
    headers: &mut http::HeaderMap,
) {
    let Some(value) = headers.get(http::header::REFRESH).cloned() else {
        return;
    };
    let Ok(text) = value.to_str() else {
        return;
    };
    let Some((prefix, url)) = text.split_once("url=") else {
        return;
    };
    let Some(rewritten) =
        rewrite_absolute_upstream_url(proxy_pass, public_origin, public_base, url.trim())
    else {
        return;
    };
    let new_value = format!("{prefix}url={rewritten}");

    headers.insert(
        http::header::REFRESH,
        new_value.parse().expect("valid refresh value"),
    );
}

fn rewrite_absolute_upstream_url(
    proxy_pass: &ProxyPass,
    public_origin: &str,
    public_base: &str,
    value: &str,
) -> Option<String> {
    let proxy_url = url::Url::parse(&proxy_pass.raw).ok()?;
    let upstream_url = url::Url::parse(value).ok()?;

    if upstream_url.scheme() != proxy_url.scheme()
        || upstream_url.host_str() != proxy_url.host_str()
        || upstream_url.port_or_known_default() != proxy_url.port_or_known_default()
    {
        return None;
    }

    let path = match proxy_pass.explicit_path_and_query() {
        Some(explicit) => rewrite_upstream_path_to_public_base(
            public_base,
            explicit.split('?').next().unwrap_or("/"),
            upstream_url.path(),
        )?,
        None => upstream_url.path().to_owned(),
    };

    Some(format!(
        "{public_origin}{}",
        append_query_and_fragment(path, upstream_url.query(), upstream_url.fragment())
    ))
}

fn rewrite_upstream_path_to_public_base(
    public_base: &str,
    upstream_base: &str,
    upstream_path: &str,
) -> Option<String> {
    let tail = upstream_path.strip_prefix(upstream_base)?;
    let mut rewritten = format!(
        "{}{}",
        public_base.trim_end_matches('/'),
        ensure_leading_slash(tail)
    );
    if public_base.ends_with('/') && tail.is_empty() {
        rewritten.push('/');
    }
    Some(rewritten)
}

fn ensure_leading_slash(segment: &str) -> String {
    if segment.is_empty() {
        String::new()
    } else if segment.starts_with('/') {
        segment.to_owned()
    } else {
        format!("/{segment}")
    }
}

fn append_query_and_fragment(path: String, query: Option<&str>, fragment: Option<&str>) -> String {
    let mut value = path;
    if let Some(query) = query {
        value.push('?');
        value.push_str(query);
    }
    if let Some(fragment) = fragment {
        value.push('#');
        value.push_str(fragment);
    }
    value
}

#[cfg(test)]
mod tests {
    use http::{HeaderValue, Response, StatusCode};

    use super::*;

    #[test]
    fn rewrite_proxy_redirect_default_rewrites_upstream_location() {
        let proxy_pass = ProxyPass {
            raw: "http://backend.example.com/base/".to_string(),
            uri: "http://backend.example.com/base/".parse().unwrap(),
            proxy_host: "backend.example.com".to_string(),
            explicit_path_and_query: Some("/base/".to_string()),
        };
        let mut response = Response::builder()
            .status(StatusCode::FOUND)
            .header(
                http::header::LOCATION,
                "http://backend.example.com/base/login",
            )
            .body(())
            .unwrap();

        rewrite_proxy_redirect_default(
            &proxy_pass,
            "https://frontend.example.com",
            "/api/",
            response.headers_mut(),
        );

        assert_eq!(
            response.headers()[http::header::LOCATION],
            HeaderValue::from_static("https://frontend.example.com/api/login")
        );
    }

    #[test]
    fn rewrite_proxy_redirect_default_keeps_foreign_location() {
        let proxy_pass = ProxyPass {
            raw: "http://backend.example.com/base/".to_string(),
            uri: "http://backend.example.com/base/".parse().unwrap(),
            proxy_host: "backend.example.com".to_string(),
            explicit_path_and_query: Some("/base/".to_string()),
        };
        let mut response = Response::builder()
            .status(StatusCode::FOUND)
            .header(http::header::LOCATION, "https://example.org/keep")
            .body(())
            .unwrap();

        rewrite_proxy_redirect_default(
            &proxy_pass,
            "https://frontend.example.com",
            "/api/",
            response.headers_mut(),
        );

        assert_eq!(
            response.headers()[http::header::LOCATION],
            HeaderValue::from_static("https://example.org/keep")
        );
    }

    #[test]
    fn rewrite_proxy_redirect_default_rewrites_refresh_url() {
        let proxy_pass = ProxyPass {
            raw: "http://backend.example.com/base/".to_string(),
            uri: "http://backend.example.com/base/".parse().unwrap(),
            proxy_host: "backend.example.com".to_string(),
            explicit_path_and_query: Some("/base/".to_string()),
        };
        let mut response = Response::builder()
            .status(StatusCode::FOUND)
            .header(
                http::header::REFRESH,
                "1; url=http://backend.example.com/base/login",
            )
            .body(())
            .unwrap();

        rewrite_proxy_redirect_default(
            &proxy_pass,
            "https://frontend.example.com",
            "/api/",
            response.headers_mut(),
        );

        assert_eq!(
            response.headers()[http::header::REFRESH],
            HeaderValue::from_static("1; url=https://frontend.example.com/api/login")
        );
    }
}
