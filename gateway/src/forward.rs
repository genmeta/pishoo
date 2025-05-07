use std::{net::SocketAddr, sync::Arc};

use bytes::Bytes;
use futures::FutureExt;
use gm_quic::{ClientParameters, Interfaces, QuicClient};
use http::StatusCode;
use http_body_util::StreamBody;
use hyper::{Request, Response, body::Frame, server::conn::http1, service::service_fn};
use hyper_util::rt::tokio::TokioIo;
use tokio::net::TcpListener;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{error, info, warn};

use crate::{
    command,
    error::CustomError,
    forward,
    localhost::TraversalFactory,
    parse::{Node, Value},
};

mod normal;
mod quic;

static ALPN: &[u8] = b"h3";

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
pub async fn serve(node: Arc<Node>) -> crate::error::Result<String> {
    let addr = if let Some(Value::Addr(addr)) = node.get("listen") {
        *addr
    } else {
        return Err(CustomError::InvalidConfig(
            "Invalid listen address".to_string(),
        ));
    };

    let listener = TcpListener::bind(addr).await.map_err(|e| {
        error!("TCP listener binding failed: {:?}", e);
        e
    })?;

    let local_addr = listener.local_addr().inspect_err(|e| {
        error!("TCP listener inspect failed: {:?}", e);
    })?;

    info!("Listening on: http://{}", local_addr);

    let resolver = if let Some(Value::Addr(resolver)) = node.get("resolver") {
        *resolver
    } else {
        unreachable!("Resolver address is required");
    };

    // 访问权限控制
    let acl = Arc::new(command::acl(&node));

    let quic_client = Arc::new(create_quic_client().await);

    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await.inspect_err(|e| {
            error!("TCP listener accept failed: {:?}", e);
        }) {
            let io = TokioIo::new(stream);
            let quic_client = quic_client.clone();
            let acl = acl.clone();

            tokio::task::spawn({
                async move {
                    // 为每个连接创建服务处理器
                    let service = service_fn(move |req| {
                        let host = validate_host(&req).unwrap();

                        if !acl.check(host) {
                            return forward::normal::proxy(req).boxed();
                        }

                        let is_connect = req.method() == "CONNECT";
                        let quic_client = quic_client.clone();
                        async move {
                            if is_connect {
                                forward::quic::connect(quic_client, req, resolver).await
                            } else {
                                forward::quic::proxy(quic_client, req, resolver).await
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
pub async fn resume(node: Arc<Node>) -> crate::error::Result<()> {
    // 获取绑定地址
    let addr = if let Some(Value::Addr(addr)) = node.get("listen") {
        *addr
    } else {
        return Err(CustomError::InvalidConfig(
            "Invalid listen address".to_string(),
        ));
    };

    // 如果 addr 正在使用, 则直接返回
    match TcpListener::bind(addr).await {
        Ok(_) => {
            let _ = serve(node).await.inspect_err(|e| {
                error!("TCP listener binding failed: {:?}", e);
            });
            return Ok(());
        }
        Err(_e) => {
            Interfaces::resume();
            info!("Resumed")
        }
    }
    Ok(())
}

/// 创建并配置 QUIC 客户端，包含 TLS 配置和网络接口绑定
async fn create_quic_client() -> QuicClient {
    let agents = [
        "1.12.74.4:20004".parse().unwrap(),
        "[2402:4e00:c011:1700:8624:7e0:5c9a:2]:20004"
            .parse()
            .unwrap(),
    ];

    let factory = TraversalFactory::with(&agents[..]);

    let mut binds = Vec::new();

    for device_ip in factory.devices().keys() {
        let device_ip = match device_ip.parse() {
            Ok(ip) => ip,
            Err(e) => {
                error!("Invalid device IP {}: {:?}", device_ip, e);
                continue;
            }
        };
        // TODO 此处使用 0 端口, 测试通过, 但不太确定是否有什么问题
        binds.push(SocketAddr::new(device_ip, 0));
    }

    tracing::debug!("QUIC client binds: {:?}", binds);

    #[allow(unused_mut)]
    let mut builder = gm_quic::QuicClient::builder_with_tls(configure_tls())
        .reuse_address()
        .with_alpns([ALPN])
        .with_iface_factory(factory);

    #[cfg(feature = "qlog")]
    {
        use std::path::PathBuf;

        use qevent::telemetry::handy::DefaultSeqLogger;

        builder = builder.with_qlog(Arc::new(DefaultSeqLogger::new(PathBuf::from("/tmp/qlog"))));
    }

    builder
        .with_parameters(create_client_params())
        .bind(&binds[..])
        .unwrap()
        .build()
}

/// 配置 QUIC 协议参数
fn create_client_params() -> ClientParameters {
    let mut params = ClientParameters::default();

    // 流控制参数
    params.set_initial_max_streams_bidi(100u32);
    params.set_initial_max_streams_uni(100u32);
    params.set_initial_max_data(1u32 << 20);
    params.set_initial_max_stream_data_uni(1u32 << 20);
    params.set_initial_max_stream_data_bidi_local(1u32 << 20);
    params.set_initial_max_stream_data_bidi_remote(1u32 << 20);
    params.set_active_connection_id_limit(10u32);

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
