use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::{Arc, OnceLock},
};

use bytes::{Buf, Bytes};
use futures::FutureExt;
use gm_quic::{ClientParameters, Pathway, QuicInterface, Socket, prelude::Endpoint};
use h3_shim::QuicClient;
use http::StatusCode;
use http_body_util::{BodyExt, combinators::BoxBody};
use hyper::{Request, Response, server::conn::http1, service::service_fn};
use hyper_util::rt::tokio::TokioIo;
use qinterface::handy::Usc;
use qtraversal::AddressRegisty;
use tokio::net::TcpListener;
use tracing::{error, info, trace, warn};

use crate::{
    dns::{AGENT, DNS_SERVER, get_or_create_addr_registry, resolve_dns},
    util::{
        body::{empty, full},
        net::pick_unused_udp_port,
    },
};

static ALPN: &[u8] = b"h3";

type BoxResponse = Response<BoxBody<Bytes, hyper::Error>>;
type H3Conn = h3::client::Connection<h3_shim::QuicConnection, Bytes>;
type H3SendRequest = h3::client::SendRequest<h3_shim::OpenStreams, Bytes>;

pub static REGISTRY: OnceLock<AddressRegisty> = OnceLock::new();

#[derive(Clone)]
pub struct LocalHost {
    registry: AddressRegisty,
    agent: SocketAddr,
    socket: Socket,
    registry_bind: SocketAddr,
}

impl LocalHost {
    async fn new() -> crate::error::Result<(Arc<QuicClient>, Self)> {
        // TODO 可能使用 IPv6
        let port = pick_unused_udp_port().expect("Failed to pick unused UDP port");
        let bind = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port));
        let registry = get_or_create_addr_registry(bind)?;
        REGISTRY.get_or_init(|| registry.clone());
        let outer = registry.outer_addr().await?;
        let registry_bind = registry.bind_addr();
        let nat_type = registry.nat_type().await?;

        info!(
            "[REGISTRY]: outer addr: {}, nat type: {:?}, bind addr: {}",
            outer, nat_type, registry_bind
        );

        let usc = Arc::new(Usc::new(registry.iface())?);
        let quic_client = create_quic_client(registry_bind, usc).await;

        let agent: SocketAddr = AGENT;
        let socket = Socket::new(registry_bind, agent);

        Ok((
            Arc::new(quic_client),
            Self {
                registry,
                agent,
                socket,
                registry_bind,
            },
        ))
    }
}

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

        let (quic_client, local_host) = match LocalHost::new().await {
            Ok((quic_client, local_host)) => (quic_client, local_host),
            Err(e) => {
                error!("Failed to initialize local host: {:?}", e);
                return Err(e);
            }
        };

        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let io = TokioIo::new(stream);
                let quic_client = quic_client.clone();

                tokio::task::spawn({
                    let local_host = local_host.clone();
                    async move {
                        let service = service_fn(move |req| {
                            let is_connect = req.method() == "CONNECT";
                            let quic_client = quic_client.clone();
                            let local_host = local_host.clone();
                            async move {
                                if is_connect {
                                    handler_connect(quic_client, local_host, req).await
                                } else {
                                    handler(quic_client, local_host, req).await
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
    local_host: LocalHost,
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
                    service_fn(move |req| handler(quic_client.clone(), local_host.clone(), req));
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
    local_host: LocalHost,
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
    let (_h3_conn, h3_request) = match create_quic_conn(quic_client, local_host, &host).await {
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
async fn create_quic_client(bind: SocketAddr, usc: Arc<Usc>) -> QuicClient {
    let params = create_client_parameters();
    let tls_config = create_tls_config();

    QuicClient::builder_with_tls(tls_config)
        .with_parameters(params)
        .reuse_interfaces()
        // .reuse_connection()
        .with_iface_binder(move |addr| {
            if addr == usc.local_addr()? {
                Ok(usc.clone())
            } else {
                Ok(Arc::new(Usc::bind(addr)?))
            }
        })
        .bind(bind)
        .expect("bind quic client")
        .build()
}

/// 利用 DNS 解析结果和本地信息建立 QUIC 连接，并构造 HTTP/3 客户端
async fn create_quic_conn(
    quic_client: Arc<QuicClient>,
    local_host: LocalHost,
    host: &str,
) -> Result<(H3Conn, H3SendRequest), Response<BoxBody<Bytes, hyper::Error>>> {
    // DNS 解析
    let remote = resolve_dns(host, DNS_SERVER.parse().unwrap())
        .await
        .map_err(|e| create_error_response(format!("DNS resolve error: {}", e)))?;

    info!("[DNS]: resolved: {} -> {:?}", host, remote);
    let outer_addr = local_host
        .registry
        .outer_addr()
        .await
        .map_err(|e| create_error_response(format!("Outer address resolve error: {}", e)))?;
    let nat_type = local_host
        .registry
        .nat_type()
        .await
        .map_err(|e| create_error_response(format!("NAT type resolve error: {}", e)))?;

    let endpoint = Endpoint::Relay {
        agent: local_host.agent,
        outer: outer_addr,
    };

    let pathway = Pathway::new(endpoint, remote);

    // QUIC 连接
    let conn = quic_client
        .connect(host, local_host.socket, pathway)
        .map_err(|e| create_error_response(format!("QUIC connect error: {:?}", e)))?;

    let _ = conn.add_address(local_host.registry_bind, outer_addr, 1, nat_type);

    // HTTP/3 客户端
    let gm_quic_conn = h3_shim::QuicConnection::new(conn).await;
    h3::client::new(gm_quic_conn)
        .await
        .map_err(|e| create_error_response(format!("H3 client error: {}", e)))
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

    trace!(
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
