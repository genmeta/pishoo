use std::{net::SocketAddr, sync::Arc};

use bytes::{Buf, Bytes};
use futures::FutureExt;
use gm_quic::ClientParameters;
use h3_shim::QuicClient;
use http::StatusCode;
use http_body_util::{BodyExt, combinators::BoxBody};
use hyper::{Request, Response, server::conn::http1, service::service_fn};
use hyper_util::rt::tokio::TokioIo;
use qinterface::handy::Usc;
use tokio::net::TcpListener;
use tracing::{error, info, trace, warn};

use crate::{
    dns::{DNS_SERVER, resolve_dns},
    localhost::ArcLocalHost,
    util::body::{empty, full},
};

static ALPN: &[u8] = b"h3";

type BoxResponse = Response<BoxBody<Bytes, hyper::Error>>;
type H3Conn = h3::client::Connection<h3_shim::QuicConnection, Bytes>;
type H3SendRequest = h3::client::SendRequest<h3_shim::OpenStreams, Bytes>;

pub struct ForwardServer;

impl ForwardServer {
    /// 启动转发服务器，绑定 TCP 监听器，接受连接并处理 HTTP/1.x 请求（支持 CONNECT 隧道）
    pub async fn serve(addr: SocketAddr) -> crate::error::Result<()> {
        let listener = match TcpListener::bind(addr).await {
            Ok(listener) => listener,
            Err(e) => {
                error!("Failed to bind TCP listener: {:?}", e);
                return Err(e.into());
            }
        };
        info!("Listening on http://{}", addr);

        let localhost = ArcLocalHost::new(addr.port());
        localhost.init_network().await;

        let quic_client = Arc::new(create_quic_client(localhost.clone()).await);

        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let io = TokioIo::new(stream);
                let quic_client = quic_client.clone();

                tokio::task::spawn({
                    let localhost = localhost.clone();
                    async move {
                        let service = service_fn(move |req| {
                            let is_connect = req.method() == "CONNECT";
                            let quic_client = quic_client.clone();
                            let localhost = localhost.clone();
                            async move {
                                if is_connect {
                                    handler_connect(quic_client, localhost, req).await
                                } else {
                                    handler(quic_client, localhost, req).await
                                }
                            }
                            .boxed()
                        });

                        if let Err(err) = http1::Builder::new()
                            .preserve_header_case(true)
                            .title_case_headers(true)
                            .serve_connection(io, service)
                            .with_upgrades()
                            .await
                        {
                            error!("Failed to serve connection: {err:?}");
                        }
                    }
                });
            }
            error!("Server error address: {addr}");
        });
        Ok(())
    }
}

/// 处理 CONNECT 请求，升级连接后建立隧道，并转发后续请求
async fn handler_connect(
    quic_client: Arc<QuicClient>,
    localhost: ArcLocalHost,
    req: Request<hyper::body::Incoming>,
) -> Result<BoxResponse, hyper::Error> {
    let uri = req.uri().to_string();

    // 验证域名
    let _host = match check_host(&req) {
        Ok(host) => host,
        Err(response) => return Ok(response),
    };

    info!("[CONNECT] request to {}", uri);

    tokio::task::spawn(async move {
        match hyper::upgrade::on(req).await {
            Ok(upgraded) => {
                info!("[CONNECT]: tunnel established to {}", uri);
                let service =
                    service_fn(move |req| handler(quic_client.clone(), localhost.clone(), req));
                if let Err(err) = http1::Builder::new()
                    .preserve_header_case(true)
                    .title_case_headers(true)
                    .serve_connection(upgraded, service)
                    .await
                {
                    error!("[CONNECT][{uri}]: Failed to serve connection: {err:?}");
                }
            }
            Err(err) => error!("Failed to upgrade connection {uri}: {err:?}"),
        }
    });

    Ok(Response::new(empty()))
}

/// 处理普通 HTTP 请求，通过 DNS 解析和 QUIC 连接转发请求，并汇总响应数据
async fn handler(
    quic_client: Arc<QuicClient>,
    localhost: ArcLocalHost,
    req: Request<hyper::body::Incoming>,
) -> Result<BoxResponse, hyper::Error> {
    let uri = req.uri().to_string();
    info!("[Forward]: request: {:?}", uri);

    // 获取并验证 host
    let host = match check_host(&req) {
        Ok(host) => host,
        Err(response) => return Ok(response),
    };
    info!("[Forward][{}]: preparing quic connection", uri);

    // 创建 QUIC 连接
    let (_h3_conn, h3_request) = match create_quic_conn(quic_client, localhost, &host).await {
        Ok(conn) => conn,
        Err(response) => return Ok(response),
    };
    info!("[Forward][{}]: quic connection established", uri);

    // 处理请求
    match proxy_request(h3_request, req).await {
        Ok(response) => Ok(response),
        Err(err) => {
            let reason = format!("[Forward][{}]: proxy request error: {}", uri, err);
            error!("{}", reason);
            Ok(create_error_response(reason))
        }
    }
}

/// 创建并配置 QUIC 客户端，包含 TLS 配置和网络接口绑定
async fn create_quic_client(localhost: ArcLocalHost) -> QuicClient {
    let params = create_client_parameters();
    let tls_config = create_tls_config();

    QuicClient::builder_with_tls(tls_config)
        .with_parameters(params)
        .reuse_interfaces()
        // .reuse_connection()
        .with_iface_binder(move |addr| {
            if let Some(iface) = localhost.iface(addr) {
                Ok(Arc::new(Usc::new(iface)?))
            } else {
                Ok(Arc::new(Usc::bind(addr)?))
            }
        })
        .build()
}

/// 利用 DNS 解析结果和本地信息建立 QUIC 连接，并构造 HTTP/3 客户端
async fn create_quic_conn(
    quic_client: Arc<QuicClient>,
    localhost: ArcLocalHost,
    host: &str,
) -> Result<(H3Conn, H3SendRequest), Response<BoxBody<Bytes, hyper::Error>>> {
    // DNS 解析
    let remotes = resolve_dns(host, DNS_SERVER.parse().unwrap())
        .await
        .map_err(|e| create_error_response(format!("DNS resolve error: {}", e)))?;
    info!("[DNS]: resolved: {} -> {:?}", host, remotes);

    const RETRY: usize = 3;

    for i in 0..RETRY {
        // TODO: server 有多个地址，按照优先级选择，重试考虑换个地址
        let index = i.min(remotes.len() - 1);
        let (pathway, socket) = localhost.match_pathway(remotes[index]).await.unwrap();

        // QUIC 连接
        let conn = quic_client
            .connect(host, socket, pathway)
            .map_err(|e| create_error_response(format!("QUIC connect error: {:?}", e)))?;

        localhost.add_direct_address(conn.clone()).await;

        // QUIC 连接
        let conn = quic_client
            .connect(host, socket, pathway)
            .map_err(|e| create_error_response(format!("QUIC connect error: {:?}", e)))?;

        // HTTP/3 客户端
        let gm_quic_conn = h3_shim::QuicConnection::new(conn).await;
        let result = h3::client::new(gm_quic_conn).await;
        match result {
            Ok(r) => return Ok(r),
            Err(e) => {
                error!("[Forward]: create h3 client error: {} retry: {}", e, i);
                if i == RETRY - 1 {
                    let _ = localhost.resume_network().await.inspect_err(|e| {
                        error!("[Forward]: resume network error: {}", e);
                    });
                    return Err(create_error_response(format!(
                        "Create h3 client error: {}",
                        e
                    )));
                }
            }
        }
    }
    Err(create_error_response(
        "Create h3 client error out of retry".to_string(),
    ))
}

/// 代理 HTTP 请求，通过 QUIC 通道发送请求数据，接收并组装响应体
async fn proxy_request(
    mut sender: H3SendRequest,
    req: Request<hyper::body::Incoming>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, Box<dyn std::error::Error + Send + Sync>> {
    let uri = req.uri().to_string();
    let (parts, body) = req.into_parts();
    let body = body.collect().await?.to_bytes();

    info!("[PROXY][{}]: sending request", uri);

    let req = http::Request::from_parts(parts, ());
    let mut stream = sender.send_request(req).await?;
    stream.send_data(body).await?;
    stream.finish().await?;

    let (mut parts, _) = stream.recv_response().await?.into_parts();
    parts.version = http::Version::HTTP_11;

    let mut response_body = Vec::new();
    let mut total_bytes = 0;

    info!("recv response {:?}", parts);
    // TODO: 流式处理响应体 https://github.com/hyperium/hyper/issues/2166

    while let Some(chunk) = stream.recv_data().await? {
        let chunk_size = chunk.chunk().len();
        total_bytes += chunk_size;
        trace!(
            "[PROXY][{}]: received chunk: {} bytes, total: {} bytes",
            uri, chunk_size, total_bytes
        );
        response_body.extend_from_slice(chunk.chunk());
    }

    info!(
        "[PROXY][{}]: response complete, total: {} bytes",
        uri, total_bytes
    );
    Ok(Response::from_parts(
        parts,
        full(Bytes::from(response_body)),
    ))
}

/// 配置客户端 QUIC 参数，设置初始流数和数据传输窗口等限制
fn create_client_parameters() -> ClientParameters {
    let mut params = ClientParameters::default();
    params.set_initial_max_streams_bidi(100u32.into());
    params.set_initial_max_streams_uni(100u32.into());
    params.set_initial_max_data((1u32 << 20).into());
    params.set_initial_max_stream_data_uni((1u32 << 20).into());
    params.set_initial_max_stream_data_bidi_local((1u32 << 20).into());
    params.set_initial_max_stream_data_bidi_remote((1u32 << 20).into());
    params
}

/// 创建 TLS 配置，设置 ALPN 协议、根证书以及密钥日志，不启用会话恢复
fn create_tls_config() -> rustls::ClientConfig {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_root_certificates(crate::common::root_cert())
        .with_no_client_auth();

    config.alpn_protocols = vec![ALPN.into()];
    config.resumption = rustls::client::Resumption::disabled();
    config.key_log = Arc::new(rustls::KeyLogFile::new());
    config
}

/// 构建错误响应，返回 SERVICE_UNAVAILABLE 状态和错误提示消息
fn create_error_response(message: String) -> Response<BoxBody<Bytes, hyper::Error>> {
    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .body(full(message))
        .unwrap()
}

/// 校验请求中的 Host 字段，确保其符合支持的域名要求，否则返回错误响应
fn check_host(
    req: &Request<hyper::body::Incoming>,
) -> Result<String, Response<BoxBody<Bytes, hyper::Error>>> {
    let host = match req.uri().host() {
        Some(host) => host,
        _ => match req.headers().get("host") {
            Some(host) => match host.to_str() {
                Ok(host) => host,
                Err(_) => {
                    warn!("[Forward]: invalid host header encoding");
                    return Err(create_error_response(
                        "Invalid host header encoding".to_string(),
                    ));
                }
            },
            None => {
                warn!("[Forward]: this host is no support {:?} ", req);
                return Err(create_error_response("Host not found".to_string()));
            }
        },
    };

    if host.ends_with("genmeta.net") {
        Ok(host.to_string())
    } else {
        warn!("[Forward]: this host is no support {:?} ", req);
        Err(create_error_response("Host not supported".to_string()))
    }
}
