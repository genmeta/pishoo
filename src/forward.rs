use std::{
    net::SocketAddr,
    sync::{Arc, OnceLock},
};

use acl::{Acl, parse_host_matches};
use bytes::Bytes;
use futures::FutureExt;
use gm_quic::{ClientParameters, QuicClient};
use http::StatusCode;
use http_body_util::StreamBody;
use hyper::{Request, Response, body::Frame, server::conn::http1, service::service_fn};
use hyper_util::rt::tokio::TokioIo;
use qinterface::handy::Usc;
use tokio::net::TcpListener;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{error, info, warn};

use crate::{error::CustomError, forward, localhost::ArcLocalHost};

mod acl;
mod normal;
mod quic;

static ALPN: &[u8] = b"h3";

static LOCALHOST: OnceLock<ArcLocalHost> = OnceLock::new();

const CHANNEL_BUFFER_SIZE: usize = 128; // 响应通道缓冲区大小

type BoxResponse = Response<StreamBody<ReceiverStream<Result<Frame<Bytes>, hyper::Error>>>>;
type H3Conn = h3::client::Connection<h3_shim::QuicConnection, Bytes>;
type H3SendRequest = h3::client::SendRequest<h3_shim::OpenStreams, Bytes>;

/// Start the QUIC proxy server
///
/// # Arguments
/// * `addr` - The listening address of the server
/// * `resolver` - The address of the DNS resolver
/// * `allow` - The list of allowed hosts
/// * `deny` - The list of denied hosts
///
/// # Returns
/// * `Result<String>` - The address the server is listening on
pub async fn serve(
    addr: SocketAddr,
    resolver: SocketAddr,
    allow: Vec<String>,
    deny: Vec<String>,
) -> crate::error::Result<String> {
    let listener = TcpListener::bind(addr).await.map_err(|e| {
        error!("TCP listener binding failed: {:?}", e);
        e
    })?;

    let local_addr = listener.local_addr().inspect_err(|e| {
        error!("TCP listener inspect failed: {:?}", e);
    })?;

    info!("Listening on: http://{}", local_addr);

    let localhost = ArcLocalHost::new(local_addr.port());
    LOCALHOST.get_or_init(|| localhost.clone());
    localhost.init_network().await;

    // Acl 规则解析
    let allow = Acl::new(parse_host_matches(allow));
    let deny = Acl::new(parse_host_matches(deny));
    let check_host = move |host: &str| {
        // Rule 1: Must match at least one allow pattern
        if !allow.check_host(host) {
            return false;
        }
        // Rule 2: If allowed, must not match any deny pattern
        if deny.check_host(host) {
            return false;
        }
        true
    };
    let check_host = Arc::new(check_host);

    let quic_client = Arc::new(create_quic_client(localhost.clone()).await);

    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let io = TokioIo::new(stream);
            let quic_client = quic_client.clone();
            let check_host = check_host.clone();

            tokio::task::spawn({
                let localhost = localhost.clone();
                async move {
                    // 为每个连接创建服务处理器
                    let service = service_fn(move |req| {
                        let host = validate_host(&req).unwrap();

                        if !check_host(host) {
                            return forward::normal::proxy(req).boxed();
                        }

                        let is_connect = req.method() == "CONNECT";
                        let quic_client = quic_client.clone();
                        let localhost = localhost.clone();
                        async move {
                            if is_connect {
                                forward::quic::connect(quic_client, localhost, req, resolver).await
                            } else {
                                forward::quic::proxy(quic_client, localhost, req, resolver).await
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

    Ok(local_addr.to_string())
}

/// Resume the network
///
/// # Returns
/// * `Result<()>` - The result of resuming the network
pub async fn resume() -> crate::error::Result<()> {
    info!("Resuming network");
    let localhost = LOCALHOST
        .get()
        .ok_or(CustomError::LocalhostNotInitialized)?;
    localhost.resume_network().await.inspect_err(|e| {
        error!("Network resume failed: {:?}", e);
    })?;
    info!("Network resumed");
    Ok(())
}

/// 创建并配置 QUIC 客户端，包含 TLS 配置和网络接口绑定
async fn create_quic_client(localhost: ArcLocalHost) -> QuicClient {
    let params = create_client_params();
    let tls_config = configure_tls();
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
        .bind(&binds[..])
        .unwrap()
        .build()
}

/// 配置 QUIC 协议参数
fn create_client_params() -> ClientParameters {
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

/// 验证请求中的 Host 头合法性
fn validate_host(req: &Request<hyper::body::Incoming>) -> Result<&str, String> {
    // 从 URI 或 Header 获取 Host
    req.uri()
        .host()
        .or_else(|| req.headers().get("host").and_then(|h| h.to_str().ok()))
        .ok_or_else(|| {
            let reason = format!("Invalid Host header: {:?}", req);
            warn!("{}", reason);
            reason
        })
}

/// 创建空响应
fn build_empty_response() -> BoxResponse {
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
fn build_error_response(message: String) -> BoxResponse {
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
