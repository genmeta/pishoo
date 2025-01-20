use std::{collections::HashMap, net::SocketAddr, str::FromStr, sync::Arc};

use bytes::{Buf, Bytes};
use h3::server::RequestStream;
use h3_shim::{BidiStream, QuicServer};
use http::{Request, Response, StatusCode, Uri, Version};
use http_body_util::BodyExt;
use hyper::client::conn::http1::Builder;
use tokio::net::TcpStream;
use tracing::{debug, error, info};

use crate::{
    error::{CustomError, Result},
    http::full,
    parse::{router::Router, rule::Rule, server::Server},
    support::TokioIo,
};

static ALPN: &[u8] = b"h3";

#[derive(Clone)]
pub struct H3Server;

impl H3Server {
    pub async fn serve(bind: SocketAddr, servers: Vec<Server>) -> Result<()> {
        let mut builder = QuicServer::builder()
            .with_supported_versions([1u32])
            .without_cert_verifier()
            .enable_sni();

        for server in servers.iter() {
            let ssl_config = if let Some(ssl_config) = &server.ssl_config {
                ssl_config
            } else {
                return Err(CustomError::Unknown);
            };

            builder = builder.add_host_with_cert_files(
                &server.server_name,
                &ssl_config.cert_path,
                &ssl_config.key_path,
            )?;
        }

        let routers: HashMap<String, Router> = servers
            .into_iter()
            .map(|s| (s.server_name, s.router))
            .collect();
        let routers = Arc::new(routers);

        let quic_server = builder.with_alpns([ALPN.to_vec()]).listen(bind)?;

        while let Ok((conn, _pathway)) = quic_server.accept().await {
            debug!(src_addr = %_pathway.local_addr(), dst_addr = %_pathway.dst_addr(), "accepted connection");

            let mut conn =
                h3::server::Connection::new(h3_shim::QuicConnection::new(conn).await).await?;
            let routers = routers.clone();
            tokio::spawn({
                async move {
                    while let Ok(Some((req, stream))) = conn.accept().await {
                        tokio::spawn({
                            let routers = routers.clone();
                            async move { handle(routers.clone(), req, stream).await }
                        });
                    }
                }
            });
        }

        Ok(())
    }
}

pub async fn handle(
    routers: Arc<HashMap<String, Router>>,
    req: Request<()>,
    stream: RequestStream<BidiStream<Bytes>, Bytes>,
) {
    if let Err(e) = handler_http3(routers, req, stream).await {
        match e {
            // TODO 这里应该有个统一的错误处理
            CustomError::Unknown => {
                debug!("unknown error");
            }
            _ => {
                debug!("error: {}", e);
            }
        }
    }
}

pub async fn handler_http3(
    routers: Arc<HashMap<String, Router>>,
    req: Request<()>,
    stream: RequestStream<BidiStream<Bytes>, Bytes>,
) -> Result<()> {
    // 提取主机名
    let host = req
        .uri()
        .authority()
        .ok_or(CustomError::MissingHost)?
        .host();
    let path = req.uri().path();

    let router = routers
        .get(host)
        .ok_or(CustomError::RouterNotFound(host.to_string()))?;
    let (pattern, rules) = router.route(path)?;

    let mut proxy_target = None;
    let mut static_root = None;

    for rule in rules {
        match rule {
            Rule::Allow(_) => {
                // TODO: 实现鉴权逻辑
            }
            Rule::Deny(_) => {
                // TODO: 实现鉴权逻辑
            }
            Rule::ProxyPass(target) => proxy_target = Some(target),
            Rule::Root(path) => static_root = Some(path),
        }
    }

    // 根据规则处理请求
    if let Some(target) = proxy_target {
        handle_proxy(&target, req, stream).await?;
    } else if let Some(root) = static_root {
        handle_static_file(root, pattern, req, stream).await?;
    } else {
        return Err(CustomError::Unknown);
    }

    Ok(())
}

pub(super) async fn handle_proxy(
    target: &str,
    req: http::Request<()>,
    mut stream: RequestStream<BidiStream<Bytes>, Bytes>,
) -> Result<()> {
    info!("proxy to {}", target);

    // 读取请求体
    let mut body = Vec::new();
    while let Some(chunk) = stream.recv_data().await? {
        body.extend_from_slice(chunk.chunk());
    }

    // 处理代理请求
    let (mut parts, _body) = req.into_parts();
    parts.uri = Uri::from_str(&format!(
        "{}{}",
        target,
        parts
            .uri
            .path_and_query()
            .map(|p| p.to_string())
            .unwrap_or_default()
    ))?;

    let uri = parts.uri.clone();
    parts.version = Version::HTTP_11;

    // TODO 修改请求头

    let req = Request::from_parts(parts, full(body));
    debug!("req: {:#?}", req);

    // 建立 TCP 连接
    let host = uri.host().ok_or(CustomError::MissingHost)?;
    let port = uri.port().map(|p| p.as_u16()).unwrap_or(80); // 默认端口 80

    let tcp_stream = TcpStream::connect((host, port)).await?;
    let io = TokioIo::new(tcp_stream);

    // 创建 HTTP 连接
    let (mut sender, conn) = Builder::new()
        .preserve_header_case(true)
        .title_case_headers(true)
        .handshake(io)
        .await?;

    // 异步处理连接
    tokio::task::spawn(async move {
        if let Err(err) = conn.await {
            error!("Connection failed: {:?}", err);
        }
    });

    // 发送请求并接收响应
    let resp = sender.send_request(req).await?;
    let (parts, body) = resp.into_parts();
    let bytes = body.collect().await?.to_bytes();

    // 发送响应
    let response = Response::from_parts(parts, ());
    stream.send_response(response).await?;

    if !bytes.is_empty() {
        stream.send_data(bytes).await?;
    }
    stream.finish().await?;

    Ok(())
}

async fn handle_static_file(
    root: String,
    pattern: String,
    req: Request<()>,
    mut stream: RequestStream<BidiStream<Bytes>, Bytes>,
) -> Result<()> {
    let path = req.uri().path().replacen(&pattern, &root, 1);
    info!("Serving static file: {}", path);

    match std::fs::read(&path) {
        Ok(body) => {
            let response = Response::builder().status(StatusCode::OK).body(())?;
            stream.send_response(response).await?;
            stream.send_data(body.into()).await?;
        }
        Err(_) => {
            let response = Response::builder().status(StatusCode::NOT_FOUND).body(())?;
            stream.send_response(response).await?;
        }
    }

    stream.finish().await?;
    Ok(())
}
