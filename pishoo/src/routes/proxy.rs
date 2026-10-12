use std::{net::SocketAddr, sync::Arc, time::Duration};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures::{FutureExt, future::FusedFuture};
use http::{Method, Request, StatusCode, Version, header};
use http_body::Frame;
use http_body_util::{BodyExt, StreamBody};
use hyper_util::rt::TokioIo;
use sha1::{Digest, Sha1};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

use crate::{Body, Error, Result, setup::ProxyLocation};

pub(crate) async fn proxy(
    route: ProxyLocation,
    mut request: Request<Body>,
) -> Result<http::Response<Body>> {
    // h3x carries the validated :protocol pseudo-header in this native extension.
    let websocket = request.method() == Method::CONNECT
        && request
            .extensions()
            .get::<Arc<str>>()
            .is_some_and(|p| p.as_ref() == "websocket");
    if websocket
        && (request
            .headers()
            .get("sec-websocket-version")
            .is_none_or(|v| v != "13")
            || request.headers().contains_key(header::TRAILER))
    {
        return Err(Error::BadRequest(
            "WebSocket requires version 13 and no trailers".into(),
        ));
    }
    let offered = websocket.then(|| request.headers().clone());
    let upload = websocket.then(|| std::mem::take(request.body_mut()));
    *request.uri_mut() = proxy_uri(&route, request.uri())?;
    let authority = request
        .uri()
        .authority()
        .ok_or_else(|| Error::BadRequest("invalid proxy authority".into()))?;
    let address = authority
        .as_str()
        .parse::<SocketAddr>()
        .map_err(|_| Error::BadRequest("invalid proxy address".into()))?;
    if request.uri().scheme_str() != Some("http") || !address.ip().is_loopback() {
        return Err(Error::BadRequest(
            "proxy target must be local HTTP/TCP".into(),
        ));
    }
    let host = authority
        .as_str()
        .parse()
        .map_err(|_| Error::BadRequest("invalid proxy authority".into()))?;
    let path = request
        .uri()
        .path_and_query()
        .map(|path| path.as_str())
        .unwrap_or("/")
        .parse()
        .map_err(|_| Error::BadRequest("invalid upstream path".into()))?;
    *request.uri_mut() = path;
    *request.version_mut() = Version::HTTP_11;
    clean_hop_headers(request.headers_mut());
    request.extensions_mut().clear();
    request.headers_mut().insert(header::HOST, host);

    let key = if websocket {
        *request.method_mut() = Method::GET;
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce).map_err(|e| Error::Io(std::io::Error::other(e)))?;
        let key = STANDARD.encode(nonce);
        let headers = request.headers_mut();
        headers.remove(header::CONTENT_LENGTH);
        headers.remove("sec-websocket-accept");
        headers.insert(header::CONNECTION, "Upgrade".parse().unwrap());
        headers.insert(header::UPGRADE, "websocket".parse().unwrap());
        headers.insert("sec-websocket-version", "13".parse().unwrap());
        headers.insert("sec-websocket-key", key.parse().unwrap());
        Some(key)
    } else {
        None
    };

    let timeout = Duration::from_secs(30);
    let stream = tokio::time::timeout(timeout, TcpStream::connect(address))
        .await
        .map_err(|_| Error::Deadline)??;
    let (mut sender, connection) = tokio::time::timeout(
        timeout,
        hyper::client::conn::http1::Builder::new().handshake(TokioIo::new(stream)),
    )
    .await
    .map_err(|_| Error::Deadline)?
    .map_err(|error| Error::Io(std::io::Error::other(error)))?;
    tokio::spawn(async move {
        if let Err(error) = connection.with_upgrades().await {
            tracing::debug!(%error, "local proxy connection ended");
        }
    });
    let mut response = tokio::time::timeout(timeout, sender.send_request(request))
        .await
        .map_err(|_| Error::Deadline)?
        .map_err(|error| Error::Io(std::io::Error::other(error)))?;
    if let (Some(key), Some(offered), Some(upload)) = (key, offered, upload) {
        if response.status() != StatusCode::SWITCHING_PROTOCOLS {
            if response.status().is_success() {
                return Err(Error::Io(std::io::Error::other(
                    "upstream did not upgrade WebSocket",
                )));
            }
            // A rejected handshake remains an ordinary HTTP response, including its body.
            clean_hop_headers(response.headers_mut());
            return Ok(response.map(|body| {
                body.map_err(|e| Box::new(e) as dhttp::BoxError)
                    .boxed_unsync()
            }));
        }
        let accept = STANDARD.encode(Sha1::digest(format!(
            "{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
        )));
        if !header_token(response.headers(), header::CONNECTION, "upgrade")
            || !header_token(response.headers(), header::UPGRADE, "websocket")
            || response
                .headers()
                .get_all("sec-websocket-accept")
                .iter()
                .count()
                != 1
            || response
                .headers()
                .get("sec-websocket-accept")
                .is_none_or(|v| v.as_bytes() != accept.as_bytes())
        {
            return Err(Error::Io(std::io::Error::other(
                "invalid upstream WebSocket handshake",
            )));
        }
        if let Some(protocol) = response.headers().get("sec-websocket-protocol") {
            let protocol = protocol
                .to_str()
                .map_err(|e| Error::Io(std::io::Error::other(e)))?;
            if protocol.is_empty()
                || protocol.contains(',')
                || response
                    .headers()
                    .get_all("sec-websocket-protocol")
                    .iter()
                    .count()
                    != 1
                || !offered
                    .get_all("sec-websocket-protocol")
                    .iter()
                    .filter_map(|v| v.to_str().ok())
                    .flat_map(|v| v.split(','))
                    .any(|value| value.trim() == protocol)
            {
                return Err(Error::Io(std::io::Error::other(
                    "unoffered upstream WebSocket protocol",
                )));
            }
        }
        let upstream = tokio::time::timeout(timeout, hyper::upgrade::on(&mut response))
            .await
            .map_err(|_| Error::Deadline)?
            .map_err(|e| Error::Io(std::io::Error::other(e)))?;
        clean_hop_headers(response.headers_mut());
        response.headers_mut().remove("sec-websocket-accept");
        response.headers_mut().remove(header::CONTENT_LENGTH);
        *response.status_mut() = StatusCode::OK;
        return Ok(response.map(|_| websocket_body(upstream, upload)));
    }
    clean_hop_headers(response.headers_mut());
    Ok(response.map(|body| {
        body.map_err(|error| Box::new(error) as dhttp::BoxError)
            .boxed_unsync()
    }))
}

fn header_token(headers: &http::HeaderMap, name: http::HeaderName, token: &str) -> bool {
    headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|v| v.trim().eq_ignore_ascii_case(token))
}

fn websocket_body(upstream: hyper::upgrade::Upgraded, mut upload: Body) -> Body {
    // The response owns both directions. Drop, EOF and I/O errors release the
    // upstream and native request body without a separate task or cancellation signal.
    let stream = async_stream::try_stream! {
        let (mut reader, mut writer) = tokio::io::split(TokioIo::new(upstream));
        let sending = async move {
            while let Some(frame) = upload.frame().await {
                let frame = frame?;
                let bytes = frame.into_data().map_err(|_| -> dhttp::BoxError {
                    Box::new(std::io::Error::other("WebSocket DATA cannot contain trailers"))
                })?;
                writer.write_all(&bytes).await?;
                // Small HA auth/ping messages must leave the buffer while the stream stays open.
                writer.flush().await?;
            }
            writer.shutdown().await?;
            Ok::<_, dhttp::BoxError>(())
        }.fuse();
        tokio::pin!(sending);
        let mut buffer = [0u8; 16 * 1024];
        loop {
            let event = tokio::select! {
                result = &mut sending, if !sending.is_terminated() => result.map(|_| None),
                result = reader.read(&mut buffer) => result.map(Some).map_err(|e| Box::new(e) as dhttp::BoxError),
            };
            if let Some(count) = event? {
                if count == 0 { break; }
                yield Frame::data(bytes::Bytes::copy_from_slice(&buffer[..count]));
            }
        }
    };
    StreamBody::new(stream).boxed_unsync()
}

pub(super) fn proxy_uri(route: &ProxyLocation, uri: &http::Uri) -> Result<http::Uri> {
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

pub(super) fn clean_hop_headers(headers: &mut http::HeaderMap) {
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
}
