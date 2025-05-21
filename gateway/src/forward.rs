use std::{io, sync::Arc, time::Duration};

use bytes::Bytes;
use futures::FutureExt;
use gm_quic::{ClientParameters, QuicClient};
use http::StatusCode;
use http_body_util::{BodyExt, Empty, Full, combinators::BoxBody};
use hyper::{Request, Response, server::conn::http1, service::service_fn};
use hyper_util::rt::tokio::TokioIo;
use qdns::Resolve;
use quic::mdns;
use tokio::net::TcpListener;
use tracing::{error, info, warn};

use crate::{
    command,
    error::CustomError,
    forward,
    parse::{Node, Value},
    pool::H3ConnectionPool,
};

mod normal;
mod quic;

static ALPN: &[u8] = b"h3";

type BoxResponse = Response<BoxBody<Bytes, io::Error>>;

/// Start the QUIC proxy server
///
/// # Arguments
/// * `node` - The configuration node
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
    let resolver: Arc<dyn Resolve + Send + Sync> =
        if let Some(Value::Resolver(resolver)) = node.get("resolver") {
            resolver.into()
        } else {
            unreachable!("Resolver address is required");
        };
    let _mdns = mdns();

    // 访问权限控制
    let acl = Arc::new(command::acl(&node));

    let quic_client = Arc::new(create_quic_client().await);
    let pool = Arc::new(H3ConnectionPool::new(quic_client));

    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await.inspect_err(|e| {
            error!("TCP listener accept failed: {:?}", e);
        }) {
            let io = TokioIo::new(stream);
            let pool = pool.clone();
            let acl = acl.clone();
            let resolver = resolver.clone();

            tokio::task::spawn({
                async move {
                    // 为每个连接创建服务处理器
                    let service = service_fn(move |req| {
                        let host = validate_host(&req).unwrap();

                        if !acl.check(host) {
                            return forward::normal::proxy(req).boxed();
                        }

                        let is_connect = req.method() == "CONNECT";
                        let pool = pool.clone();
                        let resolver = resolver.clone();
                        async move {
                            if is_connect {
                                forward::quic::connect(pool, req, resolver).await
                            } else {
                                forward::quic::proxy(pool, req, resolver).await
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
    match TcpListener::bind(addr).await {
        Ok(_) => {
            let _ = serve(node).await.inspect_err(|e| {
                error!("TCP listener binding failed: {:?}", e);
            });
            return Ok(());
        }
        Err(_e) => {
            gm_quic::resume().await;
            error!("TCP listener binding failed: {:?}", _e);
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

    // p2p 模式 client 无需绑定地址，自动扫描添加，client 也无需产生 iface， 因此不用设置 factory
    // 关键要初始化网络，设置reuse_address
    gm_quic::init_network(&agents);
    #[allow(unused_mut)]
    let mut builder = gm_quic::QuicClient::builder_with_tls(configure_tls())
        .reuse_address()
        .with_alpns([ALPN]);

    #[cfg(feature = "qlog")]
    {
        use std::path::PathBuf;

        use qevent::telemetry::handy::DefaultSeqLogger;

        builder = builder.with_qlog(Arc::new(DefaultSeqLogger::new(PathBuf::from("/tmp/qlog"))));
    }

    builder.with_parameters(create_client_params()).build()
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
    params.set_max_idle_timeout(Duration::from_secs(30));

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
            let reason = format!("Invalid Host header: {req:?}");
            warn!("{}", reason);
            reason
        })
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
