use std::{net::SocketAddr, time::Duration};

use http::{Request, Version, header};
use http_body_util::BodyExt;
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;

use crate::{Body, Error, Result, setup::ProxyLocation};

pub(crate) async fn proxy(
    route: ProxyLocation,
    mut request: Request<Body>,
) -> Result<http::Response<Body>> {
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
        let _ = connection.await;
    });
    let mut response = tokio::time::timeout(timeout, sender.send_request(request))
        .await
        .map_err(|_| Error::Deadline)?
        .map_err(|error| Error::Io(std::io::Error::other(error)))?;
    clean_hop_headers(response.headers_mut());
    Ok(response.map(|body| {
        body.map_err(|error| Box::new(error) as dhttp::BoxError)
            .boxed_unsync()
    }))
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
