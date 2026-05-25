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
    parse::{document::ConfigNode, pattern::Pattern, types::ProxyPass},
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

    debug!(headers = ?req.headers(), "processing request headers");

    let resp = pass(location, req).await?;

    debug!("sending response");
    let (mut parts, body) = resp.into_parts();

    // add custom response headers
    command::add_header(location, &mut parts);

    let resp = http::Response::from_parts(parts, body);
    Ok(super::gzip::compress_response(
        location,
        accept_encoding.as_deref(),
        resp,
    ))
}

/// Forward the request to the configured upstream.
fn pass(
    location: &Arc<ConfigNode>,
    req: Request<axum::body::Body>,
) -> impl std::future::Future<Output = Result<Response<Incoming>>> + Send {
    let location = Arc::clone(location);
    async move {
        let (parts, body) = req.into_parts();
        let proxy_pass = location
            .require::<ProxyPass>("proxy_pass")
            .whatever_context::<_, Whatever>("failed to read proxy_pass directive")?;
        let proxy_pass = &proxy_pass.0;

        tracing::debug!(%proxy_pass, "resolved proxy_pass target");

        let mut path_and_query = parts
            .uri
            .path_and_query()
            .map(|p| p.to_string())
            .unwrap_or_default();

        tracing::debug!(path_and_query, "original request path and query");

        if !proxy_pass.path().eq("/") {
            let pattern = location
                .payload::<Pattern>()
                .whatever_context::<_, Whatever>("failed to read location pattern")?
                .expect("location node should contain a pattern payload");

            match pattern.as_ref() {
                Pattern::Exact(_) | Pattern::Regex(_) | Pattern::CRegex(_) | Pattern::Common => {
                    // exact/regex: ignore proxy_pass path
                }
                Pattern::Prefix(p) | Pattern::NormalPrefix(p) => {
                    if let Some(rest) = path_and_query.strip_prefix(p.as_str()) {
                        path_and_query = format!("{}{}", proxy_pass.path(), rest);
                    }
                }
            }
        }

        tracing::info!(
            path_and_query,
            "proxying request to upstream path and query"
        );

        let target_uri = Uri::from_str(&path_and_query).whatever_context::<_, Whatever>(
            format!("failed to generate target uri from `{path_and_query}`"),
        )?;

        let mut new_parts = parts;
        new_parts.uri = target_uri.clone();
        new_parts.version = Version::HTTP_11;

        debug!(request = ?new_parts, "preparing upstream request");

        let scheme = proxy_pass
            .scheme_str()
            .expect("missing scheme in proxy_pass uri");
        let host = proxy_pass.host().expect("missing host in proxy_pass uri");
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
                let io = super::upstream_tls::connect_https(&location, proxy_pass).await?;
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
