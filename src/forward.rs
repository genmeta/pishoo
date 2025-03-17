use std::{
    net::SocketAddr,
    sync::{Arc, OnceLock},
};

use bytes::{Buf, Bytes};
use futures::FutureExt;
use gm_quic::{ClientParameters, HeartbeatConfig};
use h3_shim::QuicClient;
use http::{Method, StatusCode};
use http_body_util::{BodyExt, StreamBody};
use hyper::{
    Request, Response, body::Frame, server::conn::http1, service::service_fn, upgrade::Upgraded,
};
use hyper_util::rt::tokio::TokioIo;
use qinterface::handy::Usc;
use tokio::net::{TcpListener, TcpStream};
use tokio_stream::{StreamExt, wrappers::ReceiverStream};
use tracing::{error, info, warn};

use crate::{Resolver, dns::Dns, error::CustomError, localhost::ArcLocalHost};

static ALPN: &[u8] = b"h3";

static LOCALHOST: OnceLock<ArcLocalHost> = OnceLock::new();

const MAX_RETRY_COUNT: usize = 3; // QUIC 连接最大重试次数
const CHANNEL_BUFFER_SIZE: usize = 128; // 响应通道缓冲区大小

// 类型别名简化
type BoxResponse = Response<StreamBody<ReceiverStream<Result<Frame<Bytes>, hyper::Error>>>>;
type H3Conn = h3::client::Connection<h3_shim::QuicConnection, Bytes>;
type H3SendRequest = h3::client::SendRequest<h3_shim::OpenStreams, Bytes>;

/// 启动 TCP 监听并处理传入连接
pub async fn serve(addr: SocketAddr, resolver: SocketAddr) -> crate::error::Result<()> {
    let listener = TcpListener::bind(addr).await.map_err(|e| {
        error!("TCP listener binding failed: {:?}", e);
        e
    })?;
    info!("Listening on: http://{}", addr);

    let localhost = ArcLocalHost::new(addr.port());
    LOCALHOST.get_or_init(|| localhost.clone());
    localhost.init_network().await;

    let quic_client = Arc::new(create_quic_client(localhost.clone()).await);

    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let io = TokioIo::new(stream);
            let quic_client = quic_client.clone();

            tokio::task::spawn({
                let localhost = localhost.clone();
                async move {
                    // 为每个连接创建服务处理器
                    let service = service_fn(move |req| {
                        let (_host, normal) = validate_host(&req).unwrap();
                        if normal {
                            return normal_proxy(req).boxed();
                        }

                        let is_connect = req.method() == "CONNECT";
                        let quic_client = quic_client.clone();
                        let localhost = localhost.clone();
                        async move {
                            if is_connect {
                                handle_connect(quic_client, localhost, req, resolver).await
                            } else {
                                handle_http(quic_client, localhost, req, resolver).await
                            }
                        }
                        .boxed()
                    });

                    // 启动 HTTP/1.1 服务
                    if let Err(err) = http1::Builder::new()
                        .preserve_header_case(true)
                        .title_case_headers(true)
                        .serve_connection(io, service)
                        .with_upgrades()
                        .await
                    {
                        error!("Connection handling failed: {err:?}");
                    }
                }
            });
        }
    });

    Ok(())
}

pub async fn resume() -> crate::error::Result<()> {
    info!("Resuming network");
    let localhost = LOCALHOST
        .get()
        .ok_or(CustomError::LocalhostNotInitialized)?;
    localhost.resume_network().await?;
    Ok(())
}

/// 处理 CONNECT 隧道请求
async fn handle_connect(
    quic_client: Arc<QuicClient>,
    localhost: ArcLocalHost,
    req: Request<hyper::body::Incoming>,
    dns_server: SocketAddr,
) -> Result<BoxResponse, hyper::Error> {
    let uri = req.uri().to_string();

    // 验证主机合法性
    let _host = match validate_host(&req) {
        Ok(host) => host,
        Err(reason) => return Ok(create_error_response(reason)),
    };

    info!("[CONNECT] Establishing tunnel to {}", uri);

    // 升级连接并处理后续请求
    tokio::task::spawn(async move {
        match hyper::upgrade::on(req).await {
            Ok(upgraded) => {
                info!("[CONNECT]: tunnel established to {}", uri);
                let service = service_fn(move |req| {
                    handle_http(quic_client.clone(), localhost.clone(), req, dns_server)
                });
                if let Err(err) = http1::Builder::new()
                    .preserve_header_case(true)
                    .title_case_headers(true)
                    .serve_connection(upgraded, service)
                    .await
                {
                    error!("[CONNECT][{uri}] Connection handling failed: {err:?}");
                }
            }
            Err(err) => error!("Connection upgrade failed {uri}: {err:?}"),
        }
    });

    Ok(create_empty_response())
}

/// 处理普通 HTTP 请求
async fn handle_http(
    quic_client: Arc<QuicClient>,
    localhost: ArcLocalHost,
    req: Request<hyper::body::Incoming>,
    dns_server: SocketAddr,
) -> Result<BoxResponse, hyper::Error> {
    let uri = req.uri().to_string();
    info!("[Forward] Request: {}", uri);

    // 验证主机合法性
    let (host, _) = match validate_host(&req) {
        Ok(host) => host,
        Err(reason) => return Ok(create_error_response(reason)),
    };

    // 创建 QUIC 连接
    let (mut h3_conn, h3_request) =
        match create_quic_connection(quic_client, localhost, &host, dns_server).await {
            Ok(conn) => conn,
            Err(msg) => return Ok(create_error_response(msg)),
        };
    info!("[Forward][{}]: quic connection established", uri);

    tokio::spawn({
        let uri = uri.clone();
        async move {
            match h3_conn.wait_idle().await {
                Ok(_) => info!("[Forward][{}] QUIC connection idle", uri),
                Err(err) => error!(
                    "[Forward][{}] QUIC connection idle check failed: {}",
                    uri, err
                ),
            };
        }
    });

    // 代理请求并返回响应
    match proxy_http_request(h3_request, req).await {
        Ok(response) => {
            info!(
                "[Forward][{}] Request proxied successfully: {:?}",
                uri, response
            );
            Ok(response)
        }
        Err(err) => {
            let reason = format!("[Forward][{}] Request proxy failed: {}", uri, err);
            error!("{}", reason);
            Ok(create_error_response(reason))
        }
    }
}

/// 创建并配置 QUIC 客户端，包含 TLS 配置和网络接口绑定
async fn create_quic_client(localhost: ArcLocalHost) -> QuicClient {
    let params = configure_quic_parameters();
    let tls_config = configure_tls();
    let disable = HeartbeatConfig::disabled();
    let binds = localhost.addresses();
    QuicClient::builder_with_tls(tls_config)
        .with_parameters(params)
        .reuse_interfaces()
        .with_keylog(true)
        // .reuse_connection()
        .with_iface_binder(move |addr| {
            if let Some(iface) = localhost.iface(addr) {
                Ok(Arc::new(Usc::new(iface)?))
            } else {
                Ok(Arc::new(Usc::bind(addr)?))
            }
        })
        .defer_idle_timeout(disable)
        .bind(&binds[..])
        .unwrap()
        .build()
}

/// 创建 QUIC 连接并进行地址注册
async fn create_quic_connection(
    quic_client: Arc<QuicClient>,
    localhost: ArcLocalHost,
    host: &str,
    dns_server: SocketAddr,
) -> Result<(H3Conn, H3SendRequest), String> {
    // DNS 解析
    let dns = Dns::new(dns_server);
    let remotes = dns
        .look_up(host)
        .await
        .map_err(|e| format!("DNS resolve error: {}", e))?;
    info!("[DNS]: resolved: {} -> {:?}", host, remotes);

    for retry in 0..MAX_RETRY_COUNT {
        // TODO: server 有多个地址，按照优先级选择，重试考虑换个地址
        let index = retry.min(remotes.len() - 1);
        let Some((pathway, socket)) = localhost.match_pathway(remotes[index]).await else {
            warn!(
                "[Forward]: no pathway found for {:?} retry: {}",
                remotes[index], retry
            );
            continue;
        };

        // 建立 QUIC 连接
        let conn = quic_client
            .connect(host, socket, pathway)
            .map_err(|e| format!("QUIC connect error: {:?}", e))?;
        localhost.add_direct_address(conn.clone());

        // HTTP/3 客户端
        let gm_quic_conn = h3_shim::QuicConnection::new(conn.clone()).await;
        let result = h3::client::new(gm_quic_conn).await;
        match result {
            Ok(r) => return Ok(r),
            Err(e) => {
                error!(
                    "[Forward] H3 client creation failed: {} Retries: {}",
                    e, retry
                );
                if retry == MAX_RETRY_COUNT - 1 {
                    // 最终失败时尝试刷新网络信息
                    let _ = localhost.resume_network().await;
                    return Err(format!("H3 client creation failed: {}", e));
                }
            }
        }
    }
    error!("[Forward] Maximum retry attempts exceeded");
    Err("Maximum retry attempts exceeded".to_string())
}

/// 代理 HTTP 请求的核心逻辑
async fn proxy_http_request(
    mut send_request: H3SendRequest,
    req: Request<hyper::body::Incoming>,
) -> Result<BoxResponse, Box<dyn std::error::Error + Send + Sync>> {
    let uri = req.uri().to_string();
    let (parts, body) = req.into_parts();

    info!("[Proxy][{}] Sending request", uri);

    // 发送请求头
    let req = http::Request::from_parts(parts, ());
    let stream = send_request.send_request(req).await?;
    let (mut sender, mut recver) = stream.split();

    // 发送请求体
    tokio::spawn(async move {
        let mut body_stream = body.into_data_stream();
        while let Some(Ok(chunk)) = body_stream.next().await {
            if let Err(e) = sender.send_data(chunk).await {
                error!("Error sending request data: {}", e);
                break;
            }
        }
        if let Err(e) = sender.finish().await {
            error!("Error finishing stream: {}", e);
        };
    });

    info!("[Forward][{}] Request body sent", uri);

    // 接收响应头
    let (mut parts, _) = recver.recv_response().await?.into_parts();
    info!("[Forward] Received response headers: {:?}", parts);
    parts.version = http::Version::HTTP_11;

    // 准备响应体通道
    let (tx, rx) = tokio::sync::mpsc::channel(CHANNEL_BUFFER_SIZE);
    let body = StreamBody::new(ReceiverStream::new(rx));

    // 异步接收响应体数据
    tokio::spawn(async move {
        let _send_request = send_request.clone();
        loop {
            match recver.recv_data().await {
                Ok(Some(mut buf)) => {
                    let bytes = buf.copy_to_bytes(buf.remaining());
                    match tx.send(Ok(Frame::data(bytes))).await {
                        Ok(()) => {
                            // trace!("[Forward][{}] Sending response data frame", uri);
                        }
                        Err(_) => {
                            error!("[Proxy][{}] Failed to send response", uri);
                            break;
                        }
                    }
                }
                Ok(None) => {
                    info!("[Proxy][{}] Response received completely", uri);
                    break;
                }
                Err(err) => {
                    error!("[Proxy][{}] Data receiving error: {}", uri, err);
                    break;
                }
            }
        }
    });

    info!("[Forward] Response body receiver started");

    Ok(Response::from_parts(parts, body))
}

/// 配置 QUIC 协议参数
fn configure_quic_parameters() -> ClientParameters {
    let mut params = ClientParameters::default();
    let window_size = (1u32 << 30).into(); // 1MB 窗口大小

    // 流控制参数
    params.set_initial_max_streams_bidi(100u32.into());
    params.set_initial_max_streams_uni(100u32.into());
    params.set_initial_max_data(window_size);
    params.set_initial_max_stream_data_uni(window_size);
    params.set_initial_max_stream_data_bidi_local(window_size);
    params.set_initial_max_stream_data_bidi_remote(window_size);
    params.set_active_connection_id_limit(10);
    params.set_max_ack_delay(100);

    params
}

/// 配置 TLS 客户端参数
fn configure_tls() -> rustls::ClientConfig {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_root_certificates(crate::common::root_cert())
        .with_no_client_auth();

    // TLS 特性配置
    config.alpn_protocols = vec![ALPN.into()];
    config.resumption = rustls::client::Resumption::disabled();
    config.key_log = Arc::new(rustls::KeyLogFile::new());

    config
}

/// 创建空响应
fn create_empty_response() -> BoxResponse {
    let (tx, rx) = tokio::sync::mpsc::channel(CHANNEL_BUFFER_SIZE);
    let body = StreamBody::new(ReceiverStream::new(rx));

    // 发送空数据帧
    tokio::spawn(async move {
        let _ = tx.send(Ok(Frame::data(Bytes::new()))).await;
    });

    Response::builder()
        .status(StatusCode::OK)
        .body(body)
        .unwrap()
}

/// 创建错误响应
fn create_error_response(message: String) -> BoxResponse {
    error!("[Forward] Error response: {}", message);
    let (tx, rx) = tokio::sync::mpsc::channel(CHANNEL_BUFFER_SIZE);
    let body = StreamBody::new(ReceiverStream::new(rx));

    // 发送错误信息
    tokio::spawn(async move {
        let _ = tx.send(Ok(Frame::data(Bytes::from(message)))).await;
    });

    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .body(body)
        .unwrap()
}

/// 验证请求中的 Host 头合法性
fn validate_host(req: &Request<hyper::body::Incoming>) -> Result<(String, bool), String> {
    // 从 URI 或 Header 获取 Host
    let host = req
        .uri()
        .host()
        .or_else(|| req.headers().get("host").and_then(|h| h.to_str().ok()))
        .ok_or_else(|| {
            let reason = format!("Invalid Host header: {:?}", req);
            warn!("{}", reason);
            reason
        })?;

    // TODO 支持配置域名白名单
    // 检查域名白名单
    if host.ends_with("genmeta.net") {
        Ok((host.to_string(), false))
    } else {
        Ok((host.to_string(), true))
    }
}

async fn normal_proxy(req: Request<hyper::body::Incoming>) -> Result<BoxResponse, hyper::Error> {
    info!("[normal_proxy] req: {:?}", req);

    if Method::CONNECT == req.method() {
        if let Some(addr) = host_addr(req.uri()) {
            tokio::task::spawn(async move {
                match hyper::upgrade::on(req).await {
                    Ok(upgraded) => {
                        if let Err(e) = tunnel(upgraded, addr).await {
                            error!("server io error: {}", e);
                        };
                    }
                    Err(e) => error!("upgrade error: {}", e),
                }
            });

            Ok(create_empty_response())
        } else {
            error!("CONNECT host is not socket addr: {:?}", req.uri());
            let mut resp = create_error_response("CONNECT must be to a socket address".to_string());
            *resp.status_mut() = http::StatusCode::BAD_REQUEST;
            Ok(resp)
        }
    } else {
        let host = match req.uri().host() {
            Some(host) => host,
            None => {
                error!("no host in uri: {:?}", req.uri());
                return Ok(create_error_response("no host in uri".to_string()));
            }
        };

        let port = req.uri().port_u16().unwrap_or(80);

        let stream = match TcpStream::connect((host, port)).await {
            Ok(stream) => stream,
            Err(e) => {
                error!("connect error: {}", e);
                return Ok(create_error_response(e.to_string()));
            }
        };

        let io = TokioIo::new(stream);

        let (mut sender, conn) = hyper::client::conn::http1::Builder::new()
            .preserve_header_case(true)
            .title_case_headers(true)
            .handshake(io)
            .await?;
        tokio::task::spawn(async move {
            if let Err(err) = conn.await {
                info!("Connection failed: {:?}", err);
            }
        });

        let resp = sender.send_request(req).await?;
        let (parts, body) = resp.into_parts();
        let mut data_stream = body.into_data_stream();

        let (tx, rx) =
            tokio::sync::mpsc::channel::<std::result::Result<Frame<Bytes>, hyper::Error>>(128);
        let body_stream = StreamBody::new(ReceiverStream::new(rx));

        tokio::spawn(async move {
            while let Some(Ok(chunk)) = data_stream.next().await {
                _ = tx.send(Ok(Frame::data(chunk))).await.inspect_err(|e| {
                    error!("Error sending data frame: {:?}", e);
                });
            }
        });

        let resp = Response::from_parts(parts, body_stream);
        Ok(resp)
    }
}

fn host_addr(uri: &http::Uri) -> Option<String> {
    uri.authority().map(|auth| auth.to_string())
}

// the upgraded connection
async fn tunnel(upgraded: Upgraded, addr: String) -> std::io::Result<()> {
    // Connect to remote server
    let mut server = TcpStream::connect(addr).await?;
    let mut upgraded = TokioIo::new(upgraded);

    // Proxying data
    let (from_client, from_server) =
        tokio::io::copy_bidirectional(&mut upgraded, &mut server).await?;

    // Print message when done
    info!(
        "client wrote {} bytes and received {} bytes",
        from_client, from_server
    );

    Ok(())
}
