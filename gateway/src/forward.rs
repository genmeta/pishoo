use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
};

use bytes::Bytes;
use dhttp_home::identity::Name;
use http::{Method, StatusCode};
use http_body_util::{BodyExt, Empty, Full, combinators::UnsyncBoxBody};
use hyper::{Request, Response, server::conn::http1, service::service_fn, upgrade::OnUpgrade};
use hyper_util::rt::tokio::TokioIo;
use snafu::{Report, ResultExt, Snafu};
use tokio::{
    io,
    net::{TcpListener, TcpStream},
};
use tracing::{Instrument, debug, error, info, info_span, warn};

use crate::{
    command,
    dns::{MDNS_SERVICE, build_query_resolvers},
    error::{BoxError, Result, Whatever},
    forward,
    parse::{Node, Value},
};

pub(crate) mod h3_client;
mod normal;
mod quic;

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

/// 从配置节点中读取并设置客户端认证配置
///
/// 仅当 ssl_certificate、ssl_certificate_key 和 client_name 三者都存在且类型正确时才会设置。
/// 如果配置不完整或类型错误，会记录相应的日志。
fn setup_client_config(node: &Node) -> Result<()> {
    // 从配置中读取客户端配置(可选)
    let cert_path = node.get("ssl_certificate").and_then(|v| {
        if let Value::Path(path) = v {
            Some(path)
        } else {
            warn!("ssl_certificate must be a path, ignoring");
            None
        }
    });

    let key_path = node.get("ssl_certificate_key").and_then(|v| {
        if let Value::Path(path) = v {
            Some(path)
        } else {
            warn!("ssl_certificate_key must be a path, ignoring");
            None
        }
    });

    let client_name = node.get("client_name").and_then(|v| {
        if let Value::String(name) = v {
            Some(name.clone())
        } else {
            warn!("client_name must be a string, ignoring");
            None
        }
    });

    // 仅当所有配置项都存在时才设置客户端配置
    if let (Some(cert_path), Some(key_path), Some(client_name)) = (cert_path, key_path, client_name)
    {
        // 读取证书和密钥
        let cert_chain = std::fs::read(cert_path).whatever_context::<_, Whatever>(format!(
            "failed to read client certificate from {}",
            cert_path.display()
        ))?;
        let private_key = std::fs::read(key_path).whatever_context::<_, Whatever>(format!(
            "failed to read client private key from {}",
            key_path.display()
        ))?;

        // 设置客户端配置
        if let Err(error) =
            h3_client::set_client_config(cert_chain, private_key, client_name.clone())
        {
            info!(
                error = %Report::from_error(&error),
                "client config already set, reinitializing connection pool"
            );
        } else {
            info!(%client_name, "client config set");
        }
    } else {
        info!("client authentication not configured, using connection pool without client auth");
    }

    Ok(())
}

/// Start the QUIC proxy server
///
/// # Arguments
/// * `node` - The configuration node
///
/// # Returns
/// * `Result<String>` - The address the server is listening on
pub async fn serve(
    node: Arc<Node>,
) -> Result<(
    SocketAddr,
    impl Future<Output = Result<()>> + Send + 'static,
)> {
    tracing::info!("starting forward proxy server");
    let Some(Value::Addr(addr)) = node.get("listen").cloned() else {
        unreachable!()
    };

    let (listener, local_addr) = async {
        let listener = TcpListener::bind(addr).await?;
        let local_addr = listener.local_addr()?;
        io::Result::Ok((listener, local_addr))
    }
    .await
    .whatever_context::<_, Whatever>(format!("failed to listen to tcp address: {}", addr))?;

    info!(%local_addr, "listening on http endpoint");

    let resolvers =
        build_query_resolvers(&node, "client_name").with_mdns_resolvers(MDNS_SERVICE, |_, _| true);

    // 设置客户端认证配置
    setup_client_config(&node)?;
    h3_client::reinitialize(Some(Arc::new(resolvers.clone()))).await;

    // 访问权限控制
    let acl = Arc::new(command::acl(&node));

    let accept_tcp_stream = async move |stream: TcpStream| {
        let io = TokioIo::new(stream);
        let acl = acl.clone();

        // 为每个连接创建服务处理器
        let service = service_fn(move |mut req| {
            let acl = acl.clone();
            let span = info_span!("forward_proxy", uri=%req.uri(), method=%req.method());
            async move {
                debug!(request=?req);
                let host = match validate_host(&mut req) {
                    Ok(host) => host,
                    Err(error) => {
                        error!(error = %Report::from_error(&error), "invalid host");
                        return Ok(build_error_response(Report::from_error(&error).to_string()));
                    }
                };

                let is_connect = req.method() == Method::CONNECT;

                match acl.check(&host) {
                    true if is_connect => {
                        debug!(%host, "quic proxying connect request");
                        forward::quic::connect_tunnel(req).await
                    }
                    true => {
                        debug!(%host, "quic proxying request");
                        forward::quic::proxy(req).await
                    }
                    false if is_connect => {
                        debug!(%host, "normal proxying connect request");
                        forward::normal::connect(req).await
                    }
                    false => {
                        debug!(%host, "normal proxying request");
                        forward::normal::proxy(req).await
                    }
                }
            }
            .instrument(span)
        });

        tokio::task::spawn(
            async move {
                // 启动 HTTP/1.1 服务
                let result = http1::Builder::new()
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
    };

    let task = async move {
        loop {
            match listener.accept().await {
                Ok((stream, from)) => {
                    accept_tcp_stream(stream)
                        .instrument(info_span!("accept", %from))
                        .await
                }
                Err(error) => {
                    error!(error = %Report::from_error(&error), "listener accept failed");
                }
            }
        }
    };

    Ok((local_addr, task))
}

/// Resume the network
///
/// # Returns
/// * `Result<()>` - The result of resuming the network
pub async fn resume(node: Arc<Node>) -> Result<()> {
    match serve(node).await {
        Ok((_local_addr, forward_proxy)) => {
            tokio::spawn(
                async move {
                    // QuicInterfaces::global().clear();
                    if let Err(error) = forward_proxy.await {
                        error!(error = %Report::from_error(&error), "forward proxy failed");
                    }
                }
                .in_current_span(),
            );
            Ok(())
        }
        Err(launch_error) => {
            // 重新初始化 H3Client，清除旧连接状态
            h3_client::reinitialize(None).await;
            error!(
                error = %Report::from_error(&launch_error),
                "failed to launch forward proxy, restarting all interfaces"
            );
            Err(launch_error)
        }
    }
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

    if host.ends_with(Name::SUFFIX) {
        return None;
    }

    if let Ok(Some(name)) = Name::try_expand_from(host) {
        return Some(name.as_full().to_string());
    }

    Name::try_from_str_partial(host)
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
            Some("borber.pilot.genmeta.net")
        );
        assert_eq!(
            canonicalize_forward_host("borber.pilot~").as_deref(),
            Some("borber.pilot.genmeta.net")
        );
        assert_eq!(canonicalize_forward_host("127.0.0.1"), None);
        assert_eq!(canonicalize_forward_host("borber.pilot.genmeta.net"), None);
    }

    #[test]
    fn rewrite_request_uri_host_replaces_absolute_uri_host() {
        assert_eq!(
            rewrite_request_uri_host("http://borber.pilot/path?q=1", "borber.pilot.genmeta.net"),
            "http://borber.pilot.genmeta.net/path?q=1"
        );
    }
}
