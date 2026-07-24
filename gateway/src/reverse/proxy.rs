use std::{str::FromStr, sync::Arc};

use axum::{Extension, response::IntoResponse};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use bytes::Bytes;
use h3x::{extended_connect, qpack::field::Protocol};
use http::{
    Request, Response, StatusCode, Uri, Version,
    header::{
        CONNECTION, HOST, SEC_WEBSOCKET_EXTENSIONS, SEC_WEBSOCKET_KEY, SEC_WEBSOCKET_PROTOCOL,
        SEC_WEBSOCKET_VERSION, UPGRADE,
    },
};
use http_body_util::Empty;
use hyper::{body::Incoming, upgrade::Upgraded};
use hyper_util::rt::TokioIo;
use snafu::{Report, ResultExt, ensure_whatever};
use tokio::{
    io::{self, AsyncRead, AsyncWrite},
    net::TcpStream,
};
use tracing::{Instrument, debug, error, info, warn};

use crate::{
    command,
    error::{Result, Whatever},
    parse::{pattern::Pattern, types::ProxyPass},
    reverse::{location::LocationMatch, tunnel::TunnelIo},
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

    if is_h3_websocket_connect(&req) {
        return proxy_h3_websocket(loc, req).await;
    }

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
    if let Some(proxy_pass) = location.proxy_pass()
        && let Some(public_base) = public_base_for_proxy_redirect(loc, proxy_pass)
        && let Some(public_origin) = public_origin.as_deref()
    {
        rewrite_proxy_redirect_default(proxy_pass, public_origin, public_base, &mut parts.headers);
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
            .proxy_pass()
            .expect("proxy handler requires proxy_pass");

        tracing::debug!(proxy_pass = %proxy_pass.raw, "resolved proxy_pass target");

        let normalized = super::request_uri::normalize_request_uri(&parts.uri)
            .whatever_context::<_, Whatever>("failed to normalize request uri")?;
        let target =
            super::request_uri::build_upstream_request_target(proxy_pass, &loc, &normalized)
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
            if let Err(error) = conn.with_upgrades().await {
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

async fn proxy_h3_websocket(
    loc: &LocationMatch,
    req: Request<axum::body::Body>,
) -> Result<axum::response::Response> {
    let upstream_request = build_websocket_request(loc, &req)?;
    let (response, connect) = extended_connect::hyper::accept(req)
        .await
        .whatever_context::<_, Whatever>("failed to accept H3 WebSocket extended connect")?;
    let (upstream, headers) = establish_websocket_upstream(loc, upstream_request).await?;

    let (mut parts, body) = response.into_parts();
    copy_h3_websocket_response_headers(&mut parts.headers, &headers);
    tokio::spawn(tunnel_h3_connect(connect, upstream).in_current_span());

    Ok(http::Response::from_parts(
        parts,
        axum::body::Body::new(body),
    ))
}

fn build_websocket_request(
    loc: &LocationMatch,
    req: &Request<axum::body::Body>,
) -> Result<Request<Empty<Bytes>>> {
    let proxy_pass = loc
        .location
        .proxy_pass()
        .expect("proxy handler requires proxy_pass");
    let normalized = super::request_uri::normalize_request_uri(req.uri())
        .whatever_context::<_, Whatever>("failed to normalize WebSocket request uri")?;
    let target =
        super::request_uri::build_upstream_request_target(proxy_pass, loc, &normalized)
            .whatever_context::<_, Whatever>("failed to build WebSocket upstream request target")?;
    let target_uri = Uri::from_str(&target).whatever_context::<_, Whatever>(format!(
        "failed to generate WebSocket target uri from `{target}`"
    ))?;

    let mut websocket = Request::builder()
        .method(http::Method::GET)
        .uri(target_uri)
        .version(Version::HTTP_11)
        .body(Empty::new())
        .expect("HTTP/1.1 WebSocket request with an empty body is valid");
    let headers = websocket.headers_mut();
    *headers = req.headers().clone();
    headers.insert(
        HOST,
        http::HeaderValue::from_str(&proxy_pass.proxy_host)
            .whatever_context::<_, Whatever>("invalid WebSocket upstream host header")?,
    );
    headers.insert(CONNECTION, http::HeaderValue::from_static("Upgrade"));
    headers.insert(UPGRADE, http::HeaderValue::from_static("websocket"));
    headers
        .entry(SEC_WEBSOCKET_VERSION)
        .or_insert(http::HeaderValue::from_static("13"));
    if !headers.contains_key(SEC_WEBSOCKET_KEY) {
        headers.insert(SEC_WEBSOCKET_KEY, generated_websocket_key()?);
    }
    Ok(websocket)
}

fn generated_websocket_key() -> Result<http::HeaderValue> {
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce)
        .whatever_context::<_, Whatever>("failed to generate WebSocket handshake nonce")?;
    let encoded = STANDARD.encode(nonce);
    let value = encoded
        .parse()
        .whatever_context::<_, Whatever>("generated WebSocket handshake nonce is invalid")?;
    Ok(value)
}

async fn establish_websocket_upstream(
    loc: &LocationMatch,
    request: Request<Empty<Bytes>>,
) -> Result<(Upgraded, http::HeaderMap)> {
    let location = Arc::clone(&loc.location);
    let proxy_pass = location
        .proxy_pass()
        .expect("proxy handler requires proxy_pass");
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
                    "cannot connect to WebSocket target server {host}:{port}"
                ))?;
            send_websocket_request(io, request).await
        }
        "https" => {
            let io = super::upstream_tls::connect_https(&location, &proxy_pass.uri).await?;
            send_websocket_request(io, request).await
        }
        _ => unreachable!("unsupported proxy_pass scheme"),
    }
}

async fn send_websocket_request<I>(
    io: I,
    request: Request<Empty<Bytes>>,
) -> Result<(Upgraded, http::HeaderMap)>
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, conn) = hyper::client::conn::http1::Builder::new()
        .preserve_header_case(true)
        .title_case_headers(true)
        .handshake(TokioIo::new(io))
        .await
        .whatever_context::<_, Whatever>("failed to establish WebSocket upstream connection")?;

    tokio::spawn(
        async move {
            if let Err(error) = conn.with_upgrades().await {
                error!(error = %Report::from_error(error), "WebSocket upstream connection failed");
            }
        }
        .in_current_span(),
    );

    let mut response = sender
        .send_request(request)
        .await
        .whatever_context::<_, Whatever>("failed to send WebSocket handshake to upstream")?;
    ensure_whatever!(
        response.status() == StatusCode::SWITCHING_PROTOCOLS,
        "WebSocket upstream rejected handshake with status {}",
        response.status()
    );

    let headers = response.headers().clone();
    let upgrade = hyper::upgrade::on(&mut response);
    drop(response);
    let upgraded = upgrade
        .await
        .whatever_context::<_, Whatever>("WebSocket upstream did not upgrade the connection")?;
    Ok((upgraded, headers))
}

fn is_h3_websocket_connect<B>(request: &Request<B>) -> bool {
    request.method() == http::Method::CONNECT
        && request
            .extensions()
            .get::<Protocol>()
            .is_some_and(|protocol| protocol.as_str().eq_ignore_ascii_case("websocket"))
}

fn copy_h3_websocket_response_headers(
    destination: &mut http::HeaderMap,
    upstream: &http::HeaderMap,
) {
    for name in [SEC_WEBSOCKET_PROTOCOL, SEC_WEBSOCKET_EXTENSIONS] {
        if let Some(value) = upstream.get(&name) {
            destination.insert(name, value.clone());
        }
    }
}

async fn tunnel_h3_connect(connect: extended_connect::EstablishedConnect, upstream: Upgraded) {
    let (reader, writer) = match connect.into_streams().await {
        Ok(streams) => streams,
        Err(error) => {
            warn!(error = %Report::from_error(&error), "H3 WebSocket stream takeover failed");
            return;
        }
    };
    let client = TunnelIo::new(reader.into_reader(), writer.into_writer());
    copy_websocket_tunnel(client, TokioIo::new(upstream)).await;
}

async fn copy_websocket_tunnel<Client, Upstream>(mut client: Client, mut upstream: Upstream)
where
    Client: AsyncRead + AsyncWrite + Unpin,
    Upstream: AsyncRead + AsyncWrite + Unpin,
{
    match io::copy_bidirectional(&mut client, &mut upstream).await {
        Ok((from_client, from_upstream)) => {
            info!(
                from_client,
                from_upstream, "WebSocket proxy tunnel completed"
            );
        }
        Err(error) => {
            debug!(error = %Report::from_error(&error), "WebSocket proxy tunnel ended with IO error");
        }
    }
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
    use std::{convert::Infallible, net::SocketAddr, sync::Arc, time::Duration};

    use base64::Engine as _;
    use futures::{SinkExt, StreamExt};
    use h3x::{
        connection::ConnectionBuilder,
        dhttp::settings::Settings,
        dquic::{
            Identity, Network, QuicEndpoint,
            cert::handy::{ToCertificate, ToPrivateKey},
            net::IO,
        },
        endpoint::{H3Endpoint, hyper::TowerService},
        extended_connect::settings::EnableConnectProtocol,
    };
    use http::{
        HeaderMap, HeaderValue, Response, StatusCode, header::SEC_WEBSOCKET_ACCEPT, uri::Authority,
    };
    use http_body_util::{Empty, combinators::UnsyncBoxBody};
    use hyper::{body::Incoming, server::conn::http1, service::service_fn};
    use tokio::{net::TcpListener, task::JoinHandle, time::timeout};
    use tokio_tungstenite::{
        WebSocketStream,
        tungstenite::{Message, handshake::derive_accept_key, protocol::Role},
    };
    use tower::service_fn as tower_service_fn;

    use super::*;
    use crate::{
        parse::tests::parse_location,
        reverse::{
            access_log::ActiveAccessLog,
            location::{ConfiguredLocation, match_location},
        },
    };

    const H3_SERVER_CERT: &[u8] = include_bytes!("../../tests/fixtures/h3-localhost.cert");
    const H3_SERVER_KEY: &[u8] = include_bytes!("../../tests/fixtures/h3-localhost.key");
    type H3RequestBody = UnsyncBoxBody<Bytes, h3x::dhttp::message::MessageStreamError>;

    fn reverse_proxy_location(upstream: SocketAddr) -> LocationMatch {
        let location = parse_location(&format!("proxy_pass http://{upstream};")).unwrap();
        let configured = Arc::new(ConfiguredLocation::new(location, ActiveAccessLog::Disabled));
        match_location(&[configured], "/ws").unwrap()
    }

    async fn start_upstream_websocket_echo() -> (SocketAddr, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let service = service_fn(|mut request: Request<Incoming>| async move {
                        let key = request
                            .headers()
                            .get(SEC_WEBSOCKET_KEY)
                            .unwrap()
                            .as_bytes()
                            .to_vec();
                        let upgrade = hyper::upgrade::on(&mut request);
                        tokio::spawn(async move {
                            let upgraded = upgrade.await.unwrap();
                            let mut socket = WebSocketStream::from_raw_socket(
                                TokioIo::new(upgraded),
                                Role::Server,
                                None,
                            )
                            .await;
                            while let Some(message) = socket.next().await {
                                let message = message.unwrap();
                                if socket.send(message).await.is_err() {
                                    return;
                                }
                            }
                        });

                        let response = Response::builder()
                            .status(StatusCode::SWITCHING_PROTOCOLS)
                            .header(CONNECTION, "Upgrade")
                            .header(UPGRADE, "websocket")
                            .header(SEC_WEBSOCKET_ACCEPT, derive_accept_key(&key))
                            .body(Empty::<Bytes>::new())
                            .unwrap();
                        Ok::<_, Infallible>(response)
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .with_upgrades()
                        .await;
                });
            }
        });
        (address, task)
    }

    async fn start_h3_reverse_proxy(upstream: SocketAddr) -> (Authority, JoinHandle<()>) {
        let identity = Arc::new(Identity {
            name: "localhost".parse().unwrap(),
            certs: Arc::new(H3_SERVER_CERT.to_certificate()),
            key: Arc::new(H3_SERVER_KEY.to_private_key()),
            ocsp: Arc::new(None),
        });
        let network = Network::builder().build();
        let quic = QuicEndpoint::builder()
            .network(network.clone())
            .identity(identity)
            .bind(Arc::new(vec!["127.0.0.1:0".parse().unwrap()]))
            .build()
            .await;
        let port = network
            .quic()
            .interfaces()
            .into_iter()
            .next()
            .unwrap()
            .borrow()
            .bound_addr()
            .unwrap()
            .port();
        let authority = Authority::from_maybe_shared(format!("localhost:{port}")).unwrap();
        let settings = Settings::default().with(EnableConnectProtocol::setting(true));
        let mut endpoint = H3Endpoint::builder()
            .quic(quic)
            .builder(Arc::new(ConnectionBuilder::new(Arc::new(settings))))
            .build();
        let loc = reverse_proxy_location(upstream);
        let service = TowerService(tower_service_fn(move |request: Request<H3RequestBody>| {
            let loc = loc.clone();
            async move {
                let request = request.map(axum::body::Body::new);
                Ok::<_, Infallible>(proxy_inner(&loc, request).await.unwrap())
            }
        }));
        let task = tokio::spawn(async move {
            let _ = endpoint.listen(service).await;
        });
        (authority, task)
    }

    #[test]
    fn build_http1_websocket_request_generates_handshake_nonce() {
        let location = parse_location("proxy_pass http://127.0.0.1:3000;").unwrap();
        let configured =
            std::sync::Arc::new(ConfiguredLocation::new(location, ActiveAccessLog::Disabled));
        let loc = match_location(&[configured], "/ws").unwrap();
        let request = Request::builder()
            .method(http::Method::CONNECT)
            .uri("https://spike.dhttp.net/ws")
            .body(axum::body::Body::empty())
            .unwrap();

        let upstream = build_websocket_request(&loc, &request).unwrap();
        assert_eq!(upstream.method(), http::Method::GET);
        assert_eq!(upstream.version(), Version::HTTP_11);
        assert_eq!(upstream.uri(), "/ws");
        assert_eq!(upstream.headers()[HOST], "127.0.0.1:3000");
        assert_eq!(upstream.headers()[CONNECTION], "Upgrade");
        assert_eq!(upstream.headers()[UPGRADE], "websocket");
        assert_eq!(upstream.headers()[SEC_WEBSOCKET_VERSION], "13");

        let nonce = STANDARD
            .decode(upstream.headers()[SEC_WEBSOCKET_KEY].as_bytes())
            .unwrap();
        assert_eq!(nonce.len(), 16);
    }

    #[test]
    fn recognizes_h3_websocket_extended_connect() {
        let mut request = Request::builder()
            .method(http::Method::CONNECT)
            .uri("https://spike.dhttp.net/ws")
            .body(axum::body::Body::empty())
            .unwrap();
        request.extensions_mut().insert(Protocol::new("websocket"));

        assert!(is_h3_websocket_connect(&request));
    }

    #[tokio::test]
    async fn proxies_h3_websocket_extended_connect_and_bidirectional_frames() {
        let (upstream, upstream_task) = start_upstream_websocket_echo().await;
        let (authority, proxy_task) = start_h3_reverse_proxy(upstream).await;
        let client = H3Endpoint::new(QuicEndpoint::builder().build().await);
        let test = async {
            let connection = client.connect(authority.clone()).await.unwrap();
            let request = Request::builder()
                .method(http::Method::CONNECT)
                .uri(format!("https://{authority}/ws"))
                .extension(Protocol::new("websocket"))
                .body(Empty::<Bytes>::new())
                .unwrap();
            let response = connection
                .execute_hyper_request(request)
                .await
                .unwrap()
                .map(UnsyncBoxBody::new);
            assert_eq!(response.status(), StatusCode::OK);
            assert!(!response.headers().contains_key(SEC_WEBSOCKET_ACCEPT));

            let connect = extended_connect::hyper::establish(response).await.unwrap();
            let (reader, writer) = connect.into_streams().await.unwrap();
            let mut websocket = WebSocketStream::from_raw_socket(
                TunnelIo::new(reader.into_reader(), writer.into_writer()),
                Role::Client,
                None,
            )
            .await;
            websocket
                .send(Message::Text("proxied h3 websocket".into()))
                .await
                .unwrap();
            assert_eq!(
                websocket.next().await.unwrap().unwrap(),
                Message::Text("proxied h3 websocket".into())
            );
            websocket.close(None).await.unwrap();
        };

        let result = timeout(Duration::from_secs(10), test).await;
        upstream_task.abort();
        proxy_task.abort();
        result.unwrap();
    }

    #[test]
    fn h3_websocket_response_forwards_only_negotiated_headers() {
        let mut upstream = HeaderMap::new();
        upstream.insert(
            SEC_WEBSOCKET_ACCEPT,
            HeaderValue::from_static("http1-handshake"),
        );
        upstream.insert(
            SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("graphql-transport-ws"),
        );
        upstream.insert(
            SEC_WEBSOCKET_EXTENSIONS,
            HeaderValue::from_static("permessage-deflate"),
        );
        let mut destination = HeaderMap::new();

        copy_h3_websocket_response_headers(&mut destination, &upstream);

        assert!(!destination.contains_key(SEC_WEBSOCKET_ACCEPT));
        assert_eq!(destination[SEC_WEBSOCKET_PROTOCOL], "graphql-transport-ws");
        assert_eq!(destination[SEC_WEBSOCKET_EXTENSIONS], "permessage-deflate");
    }

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
