use std::{collections::HashMap, net::SocketAddr, str::FromStr, sync::Arc, time::Duration};

use bytes::{Buf, Bytes};
use gm_quic::{QuicInterface, prelude::handy::Usc};
use h3::server::RequestStream;
use h3_shim::{BidiStream, QuicServer};
use http::{Request, Response, Uri, Version};
use http_body_util::BodyExt;
use hyper::client::conn::http1::Builder;
use hyper_util::rt::TokioIo;
use qinterface::path::Endpoint;
use tokio::net::TcpStream;
use tracing::{debug, error, info};

use crate::{
    dns::{AGENT, DNS_SERVER, get_or_create_addr_registry, spwan_report_host_task},
    error::{CustomError, Result},
    parse::{router::Router, rule::Rule, server::ServerConfig},
    util::full,
};

const ALPN: &[u8] = b"h3";
const MAX_STREAMS: u64 = 100;
const MAX_DATA: u32 = 1 << 20;

#[derive(Clone)]
pub struct ReverseServer;

impl ReverseServer {
    /// 启动反向代理服务器，绑定到指定地址并处理服务器配置
    pub async fn serve(bind: SocketAddr, servers: Vec<ServerConfig>) -> Result<()> {
        info!("bind: {}, agent: {}", bind, AGENT);

        // 初始化网络配置
        let (outer, nat_type, usc) = init_network(bind).await?;
        let ep = Endpoint::Relay {
            agent: AGENT,
            outer,
        };

        // 初始化路由器
        let routers = init_routers(&servers, ep)?;

        // 创建并配置 QUIC 服务器
        let quic_server = create_quic_server(bind, usc, &servers)?;

        // 处理连接
        handle_connections(quic_server, bind, outer, nat_type, routers).await
    }
}

/// 初始化网络配置，获取外部地址、NAT类型和USC实例
async fn init_network(
    bind: SocketAddr,
) -> Result<(SocketAddr, qconnection::traversal::NatType, Arc<Usc>)> {
    let registry = get_or_create_addr_registry(bind)?;
    let outer = registry.outer_addr().await?;
    let nat_type = registry.nat_type().await?;
    let usc = Arc::new(Usc::new(registry.iface())?);

    info!("outer: {}, nat_type: {:?}", outer, nat_type);
    Ok((outer, nat_type, usc))
}

/// 初始化路由器，根据服务器配置创建路由表
fn init_routers(
    servers: &[ServerConfig],
    ep: Endpoint,
) -> Result<Arc<HashMap<String, Arc<Router>>>> {
    let mut routers = HashMap::new();

    for server in servers {
        let router = Arc::new(server.router.clone());
        spwan_report_host_task(server.server_name.clone(), ep, DNS_SERVER.parse()?)?;

        for name in &server.server_name {
            routers.insert(name.to_string(), router.clone());
        }
    }

    Ok(Arc::new(routers))
}

/// 创建并配置 QUIC 服务器，添加服务器证书
fn create_quic_server(
    bind: SocketAddr,
    usc: Arc<Usc>,
    servers: &[ServerConfig],
) -> Result<Arc<QuicServer>> {
    let params = create_server_params();
    let mut builder = QuicServer::builder()
        .with_supported_versions([1u32])
        .without_cert_verifier()
        .keep_alive(Duration::from_secs(30))
        .with_iface_binder(move |addr| {
            if addr == usc.local_addr()? {
                Ok(usc.clone())
            } else {
                Ok(Arc::new(Usc::bind(addr)?))
            }
        })
        .with_parameters(params)
        .enable_sni();

    // 添加服务器证书
    for server in servers {
        let cert = std::fs::read(&server.cert)?;
        let key = std::fs::read(&server.key)?;
        for name in &server.server_name {
            builder = builder.add_host(name, &*cert, &*key);
        }
    }

    Ok(builder.with_alpns([ALPN.to_vec()]).listen(bind)?)
}

/// 创建服务器参数，设置流和数据限制
fn create_server_params() -> gm_quic::ServerParameters {
    let mut params = gm_quic::ServerParameters::default();
    params.set_initial_max_streams_bidi(MAX_STREAMS);
    params.set_initial_max_streams_uni(MAX_STREAMS);
    params.set_initial_max_data(MAX_DATA.into());
    params.set_initial_max_stream_data_uni(MAX_DATA.into());
    params.set_initial_max_stream_data_bidi_local(MAX_DATA.into());
    params.set_initial_max_stream_data_bidi_remote(MAX_DATA.into());
    params
}

/// 处理 QUIC 连接，接受并处理请求
async fn handle_connections(
    quic_server: Arc<QuicServer>,
    bind: SocketAddr,
    outer: SocketAddr,
    nat_type: qconnection::traversal::NatType,
    routers: Arc<HashMap<String, Arc<Router>>>,
) -> Result<()> {
    while let Ok((conn, pathway)) = quic_server.accept().await {
        debug!(src_addr = ?pathway.local(), dst_addr = ?pathway.remote(), "accepted connection");
        let _ = conn.add_address(bind, outer, 1, nat_type);

        let mut h3_conn =
            h3::server::Connection::new(h3_shim::QuicConnection::new(conn).await).await?;
        let routers = routers.clone();

        tokio::spawn(async move {
            while let Ok(Some((req, stream))) = h3_conn.accept().await {
                let routers = routers.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_request(routers, req, stream).await {
                        debug!("Error handling request: {}", e);
                    }
                });
            }
        });
    }
    Ok(())
}

/// 处理 HTTP/3 请求，根据路由规则代理请求并发送响应
async fn handle_request(
    routers: Arc<HashMap<String, Arc<Router>>>,
    req: Request<()>,
    mut stream: RequestStream<BidiStream<Bytes>, Bytes>,
) -> Result<()> {
    let uri = req.uri().clone();
    let host = req
        .uri()
        .authority()
        .ok_or(CustomError::MissingHost)?
        .host();

    // 获取路由规则
    let router = routers
        .get(host)
        .ok_or(CustomError::RouterNotFound(host.to_string()))?;
    let (_pattern, rule) = router.route(req.uri().path())?;

    // 处理请求体
    let mut body = Vec::new();
    while let Some(chunk) = stream.recv_data().await? {
        body.extend_from_slice(chunk.chunk());
    }

    // 代理请求
    let (parts, response_body) = proxy_request(rule, &rule.proxy_pass, req, body).await?;

    // 发送响应
    stream
        .send_response(Response::from_parts(parts, ()))
        .await?;
    if !response_body.is_empty() {
        info!(
            "[{}]: sending response body: {} bytes",
            uri,
            response_body.len()
        );
        stream.send_data(response_body).await?;
    }
    stream.finish().await?;

    Ok(())
}

/// 代理请求到目标服务器，并返回响应
async fn proxy_request(
    _rule: &Rule,
    target: &str,
    req: Request<()>,
    body: Vec<u8>,
) -> Result<(http::response::Parts, Bytes)> {
    let (mut parts, _) = req.into_parts();

    // 构建代理 URI
    parts.uri = Uri::from_str(&format!(
        "{}{}",
        target,
        parts
            .uri
            .path_and_query()
            .map(|p| p.to_string())
            .unwrap_or_default()
    ))?;
    parts.version = Version::HTTP_11;

    let host = parts.uri.host().ok_or(CustomError::MissingHost)?;
    let port = parts.uri.port().map(|p| p.as_u16()).unwrap_or(80);

    // 建立连接并发送请求
    let io = TokioIo::new(TcpStream::connect((host, port)).await?);
    let (mut sender, conn) = Builder::new()
        .preserve_header_case(true)
        .title_case_headers(true)
        .handshake(io)
        .await?;

    tokio::spawn(async move {
        if let Err(err) = conn.await {
            error!("Connection failed: {:?}", err);
        }
    });

    let resp = sender
        .send_request(Request::from_parts(parts, full(body)))
        .await?;
    let (parts, body) = resp.into_parts();
    let body = body.collect().await?.to_bytes();

    info!("Response prepared: {} bytes", body.len());
    Ok((parts, body))
}
