use std::{net::SocketAddr, sync::Arc};

use bytes::Bytes;
use gm_quic::{
    prelude::BindUri,
    qinterface::iface::{QuicInterfaces, physical::PhysicalInterfaces},
};
use http::{Method, StatusCode};
use http_body_util::{BodyExt, Empty, Full, combinators::BoxBody};
use hyper::{Request, Response, server::conn::http1, service::service_fn, upgrade::OnUpgrade};
use hyper_util::rt::tokio::TokioIo;
use qdns::{HttpResolver, MdnsResolver, Resolvers};
use snafu::{Report, ResultExt};
use tokio::{
    io,
    net::{TcpListener, TcpStream},
};
use tracing::{Instrument, debug, error, info, info_span, warn};

use crate::{
    command,
    error::{Result, Whatever},
    forward,
    parse::{Node, Value},
    pool::H3ConnectionPool,
};

mod normal;
mod quic;

#[allow(dead_code)]
static ALPN: &[u8] = b"h3";

type BoxResponse = Response<BoxBody<Bytes, io::Error>>;

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
    let Some(Value::Addr(addr)) = node.get("listen").cloned() else {
        unreachable!()
    };

    let (listener, local_addr) = async {
        let listener = TcpListener::bind(addr).await?;
        let local_addr = listener.local_addr()?;
        io::Result::Ok((listener, local_addr))
    }
    .await
    .whatever_context::<_, Whatever>(format!("Failed to listen to TCP address: {}", addr))?;

    info!(target: "forward_proxy", "Listening on: http://{local_addr}");

    let mut resolvers = if let Some(Value::Resolver(resolver)) = node.get("resolver") {
        Resolvers::default().with(resolver.into())
    } else {
        Resolvers::default().with(Arc::new(
            HttpResolver::new(qdns::HTTP_DNS_SERVER)
                .whatever_context::<_, Whatever>("Failed to create http dns resolver")?,
        ))
    };

    for (device, ..) in PhysicalInterfaces::global().interfaces() {
        let socket_addr = match SocketAddr::try_from(&BindUri::from(format!(
            "iface://v4.{device}:0"
        ))) {
            Ok(socket_addr) => socket_addr,
            Err(error) => {
                warn!(target: "forward_proxy", "Failed to create mDNS resolver for device {device}: {error}" );
                continue;
            }
        };
        let SocketAddr::V4(socket_addr) = socket_addr else {
            unreachable!()
        };
        let mdns_resolver = match MdnsResolver::new(qdns::MDNS_SERVICE, *socket_addr.ip(), &device)
        {
            Ok(resolver) => resolver,
            Err(error) => {
                warn!(target: "forward_proxy", "Failed to create mDNS resolver for device {device}: {error}" );
                continue;
            }
        };
        resolvers = resolvers.with(Arc::new(mdns_resolver));
    }

    // 从配置中读取客户端配置
    let Value::Path(cert_path) = node
        .get("ssl_certificate")
        .expect("Missing ssl_certificate in proxy configuration")
    else {
        panic!("ssl_certificate must be a path");
    };

    let Value::Path(key_path) = node
        .get("ssl_certificate_key")
        .expect("Missing ssl_certificate_key in proxy configuration")
    else {
        panic!("ssl_certificate_key must be a path");
    };

    let Value::String(client_name) = node
        .get("client_name")
        .expect("Missing client_name in proxy configuration")
    else {
        panic!("client_name must be a string");
    };

    // 读取证书和密钥
    let cert_chain = std::fs::read(cert_path).whatever_context::<_, Whatever>(format!(
        "Failed to read client certificate from {}",
        cert_path.display()
    ))?;
    let private_key = std::fs::read(key_path).whatever_context::<_, Whatever>(format!(
        "Failed to read client private key from {}",
        key_path.display()
    ))?;

    // 设置客户端配置
    if let Err(e) = crate::pool::set_client_config(cert_chain, private_key, client_name.clone()) {
        info!(target: "forward", "Client config already set: {e}, will reinitialize connection pool");
    } else {
        info!(target: "forward", "Client config set with name: {client_name}");
    }

    H3ConnectionPool::reinitialize();
    // 访问权限控制
    let acl = Arc::new(command::acl(&node));

    let accept_tcp_stream = async move |stream: TcpStream| {
        let io = TokioIo::new(stream);
        let acl = acl.clone();
        let resolvers = resolvers.clone();

        // 为每个连接创建服务处理器
        let service = service_fn(move |mut req| {
            let acl = acl.clone();
            let resolvers = resolvers.clone();
            let span = info_span!(target: "forward_proxy", "forward_proxy", uri=%req.uri(), method=%req.method());
            async move {
                debug!(target: "forward_proxy", request=?req);
                let host = match validate_host(&mut req) {
                    Ok(host) => host,
                    Err(reason) => {
                        error!(target: "forward_proxy", "Invalid host: {reason}");
                        return Ok(build_error_response(reason));
                    }
                };

                let is_connect = req.method() == Method::CONNECT;

                match acl.check(&host) {
                    true if is_connect => {
                        debug!(target: "forward_proxy", "QUIC proxying CONNECT request to {host}",);
                        forward::quic::connect(req, resolvers).await
                    }
                    true => {
                        debug!(target: "forward_proxy", "QUIC proxying request to {host}");
                        forward::quic::proxy(req, resolvers).await
                    }
                    false if is_connect => {
                        debug!(target: "forward_proxy", "Normal proxying CONNECT request to {host}");
                        forward::normal::connect(req).await
                    }
                    false => {
                        debug!(target: "forward_proxy", "Normal proxying request to {host}");
                        forward::normal::proxy(req).await
                    }
                }
            }
            .instrument(span)
        });

        tokio::task::spawn(async move {
            // 启动 HTTP/1.1 服务
            if let Err(error) = http1::Builder::new()
                .preserve_header_case(true)
                .title_case_headers(true)
                .serve_connection(io, service)
                .with_upgrades()
                .await
            {
                error!(target: "forward_proxy", "Connection handling failed: {}", Report::from_error(&error));
            }
        });
    };

    let task = async move {
        loop {
            match listener.accept().await {
                Ok((stream, from)) => {
                    accept_tcp_stream(stream)
                        .instrument(info_span!(target: "forward_proxy", "accept", %from))
                        .await
                }
                Err(_) => {
                    // 出错时，继续循环以便可响应停止信号
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
            QuicInterfaces::global().clear();
            tokio::spawn(async move {
                if let Err(error) = forward_proxy.await {
                    error!(target: "forward_proxy", "Forward proxy failed: {}", Report::from_error(&error));
                }
            });
            Ok(())
        }
        Err(launch_error) => {
            H3ConnectionPool::global().clear_connections();
            QuicInterfaces::global().clear();
            error!(target: "forward_proxy", "Failed to launch forward proxy, restart all interfaces: {}.", Report::from_error(&launch_error));
            Err(launch_error)
        }
    }
}

/// 验证请求中的 Host 头合法性
fn validate_host(req: &mut Request<hyper::body::Incoming>) -> Result<String, String> {
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
            let reason = format!("Invalid Host header: {req:?}");
            warn!(target: "forward_proxy", "{}", reason);
            return Err(reason);
        }
    };

    if host.ends_with("~") {
        host = host.replacen("~", ".genmeta.net", 1);
        req.headers_mut().insert(
            http::header::HOST,
            http::HeaderValue::from_str(&host).unwrap(),
        );
        let old_uri = req.uri().clone().to_string();
        let new_uri = old_uri.replacen("~", ".genmeta.net", 1);
        *req.uri_mut() = new_uri.parse().unwrap();
    }

    Ok(host)
}

/// 创建空响应
fn build_empty_response() -> BoxResponse {
    let body = Empty::<Bytes>::new().map_err(|_| unreachable!()).boxed();

    Response::builder()
        .status(StatusCode::OK)
        .body(body)
        .unwrap()
}

/// 创建错误响应
fn build_error_response(message: String) -> BoxResponse {
    error!("[Forward] Error response: {}", message);
    let body = Full::new(Bytes::from(message))
        .map_err(|_| unreachable!())
        .boxed();

    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .body(body)
        .unwrap()
}

async fn tunnel_upgrade(request_upgrade: OnUpgrade, response_upgrade: OnUpgrade) {
    let client_io = match request_upgrade.await {
        Ok(client_io) => client_io,
        Err(error) if error.is_user() => {
            debug!(target: "forward_proxy", "Client request upgrade failed: {}", Report::from_error(&error));
            return;
        }
        Err(error) => {
            error!(target: "forward_proxy", "Client request upgrade failed: {}", Report::from_error(&error));
            return;
        }
    };
    let server_io = match response_upgrade.await {
        Ok(server_io) => server_io,
        Err(error) if error.is_user() => {
            debug!(target: "forward_proxy", "Server response upgrade failed: {}", Report::from_error(&error));
            return;
        }
        Err(error) => {
            error!(target: "forward_proxy", "Server response upgrade failed: {}", Report::from_error(&error));
            return;
        }
    };

    tracing::debug!(target: "forward_proxy", "Upgraded proxy started");
    match io::copy_bidirectional(&mut TokioIo::new(client_io), &mut TokioIo::new(server_io)).await {
        Ok((from_client, from_server)) => {
            info!(
                target: "forward_proxy",
                "Upgraded proxy done: client wrote {from_client} bytes and received {from_server} bytes",
            );
        }
        Err(error) => {
            error!(target: "forward_proxy", "Upgraded proxy aborted: {}", Report::from_error(&error));
        }
    }
}
