use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use bytes::Bytes;
use dhttp::{
    endpoint::Endpoint,
    name::{DhttpName, Name},
};
use http::{Method, StatusCode};
use http_body_util::{BodyExt, Empty, Full, combinators::UnsyncBoxBody};
use hyper::{Request, Response, server::conn::http1, service::service_fn, upgrade::OnUpgrade};
use hyper_util::rt::tokio::TokioIo;
use snafu::{OptionExt, Report, ResultExt, Snafu};
use tokio::{
    io,
    net::{TcpListener, TcpStream},
    sync::Semaphore,
};
use tracing::{Instrument, debug, error, info, info_span, warn};

use crate::{
    command,
    error::{BoxError, Result, Whatever},
    forward,
    parse::{document::ConfigNode, types::SocketAddrs},
};

mod normal;
mod quic;
mod task_scope;

use task_scope::ForwardTaskScope;

#[allow(dead_code)]
pub static ALPN: &[u8] = b"h3";

type BoxResponse = Response<UnsyncBoxBody<Bytes, BoxError>>;

#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum ForwardRequestError {
    #[snafu(display("invalid host header in request"))]
    InvalidHostHeader,

    #[snafu(display("missing host in request uri"))]
    MissingHostInUri,

    #[snafu(display("connect request must target a valid host"))]
    MissingConnectAuthority,
}

/// Configure TCP keepalive on a stream to detect dead peers.
///
/// After 60 seconds of idle, sends probes every 10 seconds; 3 consecutive
/// failures trigger a RST (~90 seconds total).
fn configure_tcp_keepalive(stream: &TcpStream) {
    let sock = socket2::SockRef::from(stream);
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(Duration::from_secs(60))
        .with_interval(Duration::from_secs(10))
        .with_retries(3);
    if let Err(e) = sock.set_tcp_keepalive(&keepalive) {
        warn!(error = %e, "failed to set TCP keepalive");
    }
}

/// Start the QUIC proxy server
///
/// # Arguments
/// * `node` - The configuration node
/// * `client` - DHTTP endpoint used for outbound QUIC proxying
///
/// # Returns
/// * `Result<(SocketAddr, impl Future)>` - The address and server task
pub async fn serve(
    node: Arc<ConfigNode>,
    client: Arc<Endpoint>,
) -> Result<(
    SocketAddr,
    impl Future<Output = Result<()>> + Send + 'static,
)> {
    tracing::info!("starting forward proxy server");
    let listen = node
        .require::<SocketAddrs>("listen")
        .whatever_context::<_, Whatever>("failed to read forward proxy listen directive")?;
    let addr = *listen
        .0
        .first()
        .whatever_context::<_, Whatever>("missing forward proxy listen address")?;

    let (listener, local_addr) = async {
        let listener = TcpListener::bind(addr).await?;
        let local_addr = listener.local_addr()?;
        io::Result::Ok((listener, local_addr))
    }
    .await
    .whatever_context::<_, Whatever>(format!("failed to listen to tcp address: {}", addr))?;

    info!(%local_addr, "listening on http endpoint");

    // 访问权限控制
    let acl = Arc::new(command::acl(&node));
    let semaphore = Arc::new(Semaphore::new(1024));
    let task_scope = ForwardTaskScope::new();
    let task_spawner = task_scope.spawner();

    let task = async move {
        loop {
            match listener.accept().await {
                Ok((stream, from)) => {
                    let Ok(permit) = semaphore.clone().acquire_owned().await else {
                        break; // semaphore closed
                    };
                    let acl = acl.clone();
                    let client = client.clone();
                    let task_spawner = task_spawner.clone();
                    async {
                        let _permit = permit;
                        configure_tcp_keepalive(&stream);
                        let io = TokioIo::new(stream);

                        let service_task_spawner = task_spawner.clone();
                        // 为每个连接创建服务处理器
                        let service = service_fn(move |mut req| {
                            let acl = acl.clone();
                            let client = client.clone();
                            let request_task_spawner = service_task_spawner.clone();
                            let span =
                                info_span!("forward_proxy", uri=%req.uri(), method=%req.method());
                            async move {
                                debug!(request=?req);
                                let host = match validate_host(&mut req) {
                                    Ok(host) => host,
                                    Err(error) => {
                                        error!(
                                            error = %Report::from_error(&error),
                                            "invalid host"
                                        );
                                        return Ok(build_error_response(
                                            Report::from_error(&error).to_string(),
                                        ));
                                    }
                                };

                                let is_connect = req.method() == Method::CONNECT;

                                match acl.check(&host) {
                                    true if is_connect => {
                                        debug!(%host, "quic proxying connect request");
                                        forward::quic::connect_tunnel(
                                            req,
                                            client,
                                            request_task_spawner,
                                        )
                                        .await
                                    }
                                    true => {
                                        debug!(%host, "quic proxying request");
                                        forward::quic::proxy(req, client, request_task_spawner)
                                            .await
                                    }
                                    false if is_connect => {
                                        debug!(%host, "normal proxying connect request");
                                        forward::normal::connect(req, request_task_spawner).await
                                    }
                                    false => {
                                        debug!(%host, "normal proxying request");
                                        forward::normal::proxy(req, request_task_spawner).await
                                    }
                                }
                            }
                            .instrument(span)
                        });

                        task_spawner.spawn(
                            async move {
                                let result = http1::Builder::new()
                                    .timer(hyper_util::rt::tokio::TokioTimer::new())
                                    .header_read_timeout(Some(Duration::from_secs(120)))
                                    .preserve_header_case(true)
                                    .title_case_headers(true)
                                    .serve_connection(io, service)
                                    .with_upgrades()
                                    .await;
                                match &result {
                                    Ok(()) => info!("http/1.1 serve_connection completed"),
                                    Err(error) => {
                                        error!(
                                            error = %Report::from_error(error),
                                            canceled = error.is_canceled(),
                                            closed = error.is_closed(),
                                            parse = error.is_parse(),
                                            user = error.is_user(),
                                            incomplete_message = error.is_incomplete_message(),
                                            "http/1.1 serve_connection failed"
                                        );
                                    }
                                }
                            }
                            .in_current_span(),
                        );
                    }
                    .instrument(info_span!("accept", %from))
                    .await
                }
                Err(error) => {
                    error!(error = %Report::from_error(&error), "listener accept failed");
                    tokio::time::sleep(Duration::from_millis(33)).await;
                }
            }
        }
        task_scope.shutdown().await;
        Ok(())
    };

    Ok((local_addr, task))
}

/// 验证请求中的 Host 头合法性
fn validate_host(req: &mut Request<hyper::body::Incoming>) -> Result<String, ForwardRequestError> {
    let mut host = req.uri().host().map(String::from);
    if host.is_none() {
        host = req
            .headers()
            .get(http::header::HOST)
            .and_then(|h| h.to_str().ok().map(String::from));
    }

    let mut host = match host {
        Some(h) => h,
        None => {
            return Err(ForwardRequestError::InvalidHostHeader);
        }
    };

    if let Some(canonical_host) = canonicalize_forward_host(&host) {
        host = canonical_host;
        if let Ok(hv) = http::HeaderValue::from_str(&host) {
            req.headers_mut().insert(http::header::HOST, hv);
        }
        let old_uri = req.uri().to_string();
        let new_uri = rewrite_request_uri_host(&old_uri, &host);
        if let Ok(parsed) = new_uri.parse() {
            *req.uri_mut() = parsed;
        }
    }

    Ok(host)
}

fn canonicalize_forward_host(host: &str) -> Option<String> {
    if host.parse::<IpAddr>().is_ok() || host.eq_ignore_ascii_case("localhost") {
        return None;
    }

    if host.len() >= DhttpName::SUFFIX.len()
        && host[host.len() - DhttpName::SUFFIX.len()..].eq_ignore_ascii_case(DhttpName::SUFFIX)
    {
        let name = Name::try_from(host).ok()?;
        let name = DhttpName::try_from(name).ok()?;
        return (host != name.as_full()).then(|| name.as_full().to_owned());
    }

    DhttpName::try_from(host)
        .ok()
        .map(|name| name.as_full().to_string())
}

fn rewrite_request_uri_host(uri: &str, host: &str) -> String {
    if let Some(stripped) = uri.strip_prefix("http://") {
        if let Some((_, remain)) = stripped.split_once('/') {
            return format!("http://{host}/{}", remain);
        }
        return format!("http://{host}");
    }

    if let Some(stripped) = uri.strip_prefix("https://") {
        if let Some((_, remain)) = stripped.split_once('/') {
            return format!("https://{host}/{}", remain);
        }
        return format!("https://{host}");
    }

    uri.to_string()
}

/// 创建空响应
fn build_empty_response() -> BoxResponse {
    let body = Empty::<Bytes>::new()
        .map_err(|_| unreachable!())
        .boxed_unsync();

    Response::builder()
        .status(StatusCode::OK)
        .body(body)
        .unwrap()
}

/// 创建错误响应
fn build_error_response(message: String) -> BoxResponse {
    let body = Full::new(Bytes::from(message))
        .map_err(|_| unreachable!())
        .boxed_unsync();

    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .body(body)
        .unwrap()
}

async fn tunnel_upgrade(request_upgrade: OnUpgrade, response_upgrade: OnUpgrade) {
    let client_io = match request_upgrade.await {
        Ok(client_io) => client_io,
        Err(error) if error.is_user() => {
            debug!(error = %Report::from_error(&error), "client request upgrade failed");
            return;
        }
        Err(error) => {
            error!(error = %Report::from_error(&error), "client request upgrade failed");
            return;
        }
    };
    let server_io = match response_upgrade.await {
        Ok(server_io) => server_io,
        Err(error) if error.is_user() => {
            debug!(error = %Report::from_error(&error), "server response upgrade failed");
            return;
        }
        Err(error) => {
            error!(error = %Report::from_error(&error), "server response upgrade failed");
            return;
        }
    };

    tracing::debug!("upgraded proxy started");
    match io::copy_bidirectional(&mut TokioIo::new(client_io), &mut TokioIo::new(server_io)).await {
        Ok((from_client, from_server)) => {
            info!(from_client, from_server, "upgraded proxy completed");
        }
        Err(error) => {
            error!(error = %Report::from_error(&error), "upgraded proxy aborted");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalize_forward_host_expands_partial_and_tilde() {
        assert_eq!(
            canonicalize_forward_host("borber.pilot").as_deref(),
            Some("borber.pilot.dhttp.net")
        );
        assert_eq!(
            canonicalize_forward_host("borber.pilot~").as_deref(),
            Some("borber.pilot.dhttp.net")
        );
        assert_eq!(
            canonicalize_forward_host("BORBER.PILOT~").as_deref(),
            Some("borber.pilot.dhttp.net")
        );
        assert_eq!(
            canonicalize_forward_host("Borber.Pilot.Dhttp.Net").as_deref(),
            Some("borber.pilot.dhttp.net")
        );
        assert_eq!(canonicalize_forward_host("127.0.0.1"), None);
        assert_eq!(canonicalize_forward_host("borber.pilot.dhttp.net"), None);
    }

    #[test]
    fn rewrite_request_uri_host_replaces_absolute_uri_host() {
        assert_eq!(
            rewrite_request_uri_host("http://borber.pilot/path?q=1", "borber.pilot.dhttp.net"),
            "http://borber.pilot.dhttp.net/path?q=1"
        );
    }
}
