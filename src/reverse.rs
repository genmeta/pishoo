use std::{collections::HashMap, net::SocketAddr, str::FromStr, sync::Arc};

use bytes::{Buf, Bytes};
use futures::StreamExt;
use gm_quic::{HeartbeatConfig, prelude::handy::Usc};
use h3::server::RequestStream;
use h3_shim::{BidiStream, QuicServer, RecvStream};
use http::{Request, Response, StatusCode, Uri, Version};
use http_body_util::{BodyExt, StreamBody};
use hyper::{
    body::{Frame, Incoming},
    client::conn::http1::Builder,
};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, error, info, warn};

use crate::{
    dns::Dns,
    error::{CustomError, Result},
    localhost::ArcLocalHost,
    parse::{router::Router, rule::Rule, server::ServerConfig},
};

// 协议配置常量
const ALPN: &[u8] = b"h3"; // 应用层协议协商标识
const MAX_STREAMS: u64 = 100; // 最大双向/单向流数量
const MAX_DATA: u32 = 1 << 30; // 最大数据限制 (1MB)

pub async fn serve(bind: SocketAddr, servers: Vec<ServerConfig>) -> Result<()> {
    let localhost = ArcLocalHost::new(bind.port());
    localhost.init_network().await;
    let routers = init_routers(&servers, localhost.clone())?;

    // 创建并配置 QUIC 服务器
    let quic_server = create_quic_server(localhost.clone(), &servers)?;

    // 处理连接
    handle_connections(quic_server, localhost, routers).await
}

/// 初始化路由器，根据服务器配置创建路由表
fn init_routers(
    servers: &[ServerConfig],
    localhost: ArcLocalHost,
) -> Result<Arc<HashMap<String, Arc<Router>>>> {
    let mut routers = HashMap::new();
    for server in servers {
        let router = Arc::new(server.router.clone());
        let resolver = Dns::new(server.resolver);
        resolver.spwan_publish(server.server_name.clone(), localhost.clone());
        for name in &server.server_name {
            routers.insert(name.to_string(), router.clone());
        }
    }

    Ok(Arc::new(routers))
}

/// 创建QUIC服务器实例
fn create_quic_server(
    localhost: ArcLocalHost,
    servers: &[ServerConfig],
) -> Result<Arc<QuicServer>> {
    let params = create_server_params();
    let disabled_keep_alive = HeartbeatConfig::disabled();
    let local_host = localhost.clone();
    let mut builder = QuicServer::builder()
        .with_supported_versions([1u32]) // 支持QUIC版本1
        .without_cert_verifier() // 禁用证书验证
        .with_iface_binder(move |addr| {
            if let Some(iface) = local_host.iface(addr) {
                warn!("bind addr {}", addr);
                Ok(Arc::new(Usc::new(iface)?))
            } else {
                warn!("bind addr error");
                Ok(Arc::new(Usc::bind(addr)?))
            }
        })
        .with_parameters(params)
        .defer_idle_timeout(disabled_keep_alive)
        .enable_sni();

    // 为每个服务器添加TLS证书
    for server in servers {
        let cert = std::fs::read(&server.cert)?;
        let key = std::fs::read(&server.key)?;
        for domain in &server.server_name {
            builder = builder.add_host(domain, &*cert, &*key);
        }
    }

    let binds = localhost.addresses();
    info!("binds {:?}", binds);
    Ok(builder
        .with_alpns([ALPN.to_vec()])
        .listen(&*binds)
        .inspect_err(|e| {
            error!("listen err {:?}", e);
        })?)
}

/// 创建QUIC服务器参数配置
fn create_server_params() -> gm_quic::ServerParameters {
    let mut params = gm_quic::ServerParameters::default();
    params.set_initial_max_streams_bidi(MAX_STREAMS); // 双向流限制
    params.set_initial_max_streams_uni(MAX_STREAMS); // 单向流限制
    params.set_initial_max_data(MAX_DATA.into()); // 连接总数据限制
    params.set_initial_max_stream_data_uni(MAX_DATA.into());
    params.set_initial_max_stream_data_bidi_local(MAX_DATA.into());
    params.set_initial_max_stream_data_bidi_remote(MAX_DATA.into());
    params.set_active_connection_id_limit(10); // 允许多路径同时打洞
    params.set_max_ack_delay(100);
    params
}

/// 处理客户端连接
async fn handle_connections(
    quic_server: Arc<QuicServer>,
    localhost: ArcLocalHost,
    routers: Arc<HashMap<String, Arc<Router>>>,
) -> Result<()> {
    // 持续接受新连接
    while let Ok((conn, pathway)) = quic_server.accept().await {
        debug!(src_addr = ?pathway.local(), dst_addr = ?pathway.remote(), "accepted connection");
        localhost.add_direct_address(conn.clone());

        // 将QUIC连接包装为H3 QUIC连接
        let h3_quic_conn = h3_shim::QuicConnection::new(conn).await;

        // 建立H3连接
        let mut h3_conn = match h3::server::Connection::new(h3_quic_conn).await {
            Ok(conn) => {
                info!("[Handle Conn] H3 connection established");
                conn
            }
            Err(e) => {
                error!(
                    "[Handle Conn] Failed to establish H3 connection | detail: {}",
                    e
                );
                continue;
            }
        };

        // 为每个连接创建异步任务
        tokio::spawn({
            let routers_clone = routers.clone();
            async move {
                while let Ok(Some((req, stream))) = h3_conn
                    .accept()
                    .await
                    .inspect_err(|e| error!("Connection acceptance error | detail: {:?}", e))
                {
                    let routers = routers_clone.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_request(routers, req, stream).await {
                            error!("Request processing error | detail: {}", e);
                        }
                    });
                }
            }
        });
    }
    Ok(())
}

/// 处理单个HTTP请求
async fn handle_request(
    routers: Arc<HashMap<String, Arc<Router>>>,
    req: Request<()>,
    stream: RequestStream<BidiStream<Bytes>, Bytes>,
) -> Result<()> {
    let uri = req.uri().clone();
    let host = req
        .uri()
        .authority()
        .ok_or(CustomError::MissingHost)?
        .host();

    // 查找匹配的路由规则
    let router = routers
        .get(host)
        .ok_or_else(|| CustomError::RouterNotFound(host.to_string()))?;
    let (_pattern, rule) = router.route(req.uri().path())?;

    let (mut sender, receiver) = stream.split();

    // 代理请求并获取响应
    match proxy_request(rule, req, receiver).await {
        Ok(resp) => {
            info!("[Response handling][{}] Sending response", uri);
            let (parts, body) = resp.into_parts();

            // 发送响应头
            sender
                .send_response(Response::from_parts(parts, ()))
                .await?;

            info!("[Response handling][{}] Sending response headers", uri);

            // 流式发送响应体
            let mut body_stream = body.into_data_stream();
            while let Some(Ok(chunk)) = body_stream.next().await {
                sender.send_data(chunk).await?;
            }
            info!("sent response body completely");
        }
        Err(e) => {
            error!("[Response handling] Proxy error | detail: {}", e);
            // 构造错误响应
            let resp = Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .body(())
                .unwrap();

            sender.send_response(resp).await?;
            sender.send_data(e.to_string().into()).await?;
        }
    };

    // 结束流
    info!("[Response handling][{}] Closing stream", uri);
    sender.finish().await?;
    info!("[Response handling][{}] Processing completed", uri);
    Ok(())
}

/// 执行实际代理请求
async fn proxy_request(
    rule: &Rule,
    req: Request<()>,
    mut receiver: RequestStream<RecvStream, Bytes>,
) -> Result<Response<Incoming>> {
    // 构造目标URI
    let (parts, _) = req.into_parts();
    let target_uri = Uri::from_str(&format!(
        "{}{}",
        rule.proxy_pass,
        parts
            .uri
            .path_and_query()
            .map(|p| p.to_string())
            .unwrap_or_default()
    ))?;

    // 准备请求参数
    let mut new_parts = parts;
    new_parts.uri = target_uri.clone();
    new_parts.version = Version::HTTP_11;

    // 解析目标地址
    let host = new_parts.uri.host().ok_or(CustomError::MissingHost)?;
    let port = new_parts.uri.port().map(|p| p.as_u16()).unwrap_or(80);

    // 建立TCP连接
    let io =
        TokioIo::new(TcpStream::connect((host, port)).await.inspect_err(|e| {
            error!("TCP connection error: {}:{} | detail: {:?}", host, port, e)
        })?);

    // 创建HTTP客户端连接
    let (mut sender, conn) = Builder::new()
        .preserve_header_case(true) // 保持首字母大小写
        .title_case_headers(true) // 标题首字母大写
        .handshake(io)
        .await?;

    info!(
        "[Request processing] HTTP client connection established | target: {:?}",
        target_uri
    );

    // 启动连接维护任务
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            error!("[Proxy connection] Maintenance failed | detail: {:?}", e);
        }
    });

    // 创建请求体通道
    let (tx, rx) =
        tokio::sync::mpsc::channel::<std::result::Result<Frame<Bytes>, hyper::Error>>(128);
    let body = StreamBody::new(ReceiverStream::new(rx));

    // 异步转发请求体数据
    tokio::spawn(async move {
        while let Ok(Some(chunk)) = receiver.recv_data().await.inspect_err(|e| {
            error!(
                "[Request processing] Request body reception error | detail: {:?}",
                e
            );
        }) {
            let mut data = chunk.chunk();
            debug!(
                "[Request processing] Sending request body | data_length: {}",
                data.len()
            );
            let _ = tx
                .send(Ok(Frame::data(data.copy_to_bytes(data.len()))))
                .await
                .inspect_err(|e| {
                    error!(
                        "[Request processing] Request body send error | detail: {:?}",
                        e
                    )
                });
        }
    });

    // 发送代理请求
    let response = sender
        .send_request(Request::from_parts(new_parts, body))
        .await?;

    info!("[Request processing] Finished sending request body");

    Ok(response)
}
