use std::{collections::HashMap, net::SocketAddr, path::PathBuf, sync::Arc};

use bytes::Bytes;
use gm_quic::QuicServer;
use h3::server::RequestStream;
use h3_shim::BidiStream;
use http::{Request, Response, StatusCode};
use qevent::telemetry::handy::DefaultSeqLogger;
use tracing::{debug, error, info};

use crate::{
    dns::Dns,
    error::{CustomError, Result},
    localhost::TraversalFactory,
    parse::{Node, Value},
    reverse,
};

mod file;
mod proxy;
#[cfg(feature = "sshd")]
mod sshd;

const ALPN: &[u8] = b"h3"; // 应用层协议协商标识

type RouterMap = Arc<HashMap<String, Arc<Node>>>;
type ResolverList = Vec<(String, SocketAddr)>;

/// Start the QUIC proxy server
///
/// # Arguments
/// * `bind` - The listening address of the server
/// * `servers` - The list of server configurations
///
/// # Returns
/// * `Result<()>` - An empty result if successful, or an error if failed
pub async fn serve(bind: SocketAddr, servers: Vec<Arc<Node>>) -> Result<String> {
    let (routers, resolvers) = init_routers(&servers)?;
    let (quic_server, binds) = create_quic_server(bind, &servers)?;
    for (server_name, resolver) in resolvers {
        Dns::add_record(server_name, resolver, binds.clone());
    }
    handle_connections(quic_server, routers).await?;
    Ok("Server exited".to_string())
}

/// 初始化路由器，根据服务器配置创建路由表
fn init_routers(servers: &[Arc<Node>]) -> Result<(RouterMap, ResolverList)> {
    let mut routers = HashMap::new();
    let mut resolvers = vec![];
    for server in servers {
        let resolver = if let Some(Value::Addr(resolver)) = server.get("resolver") {
            *resolver
        } else {
            unreachable!("Invalid resolver address");
        };

        let server_name = if let Some(Value::StringVec(server_name)) = server.get("server_name") {
            server_name.clone()
        } else {
            unreachable!("Invalid server name");
        };

        for name in server_name {
            resolvers.push((name.to_string(), resolver));
            routers.insert(name.to_string(), server.clone());
        }
    }

    Ok((Arc::new(routers), resolvers))
}

/// 创建QUIC服务器实例
fn create_quic_server(
    bind: SocketAddr,
    servers: &[Arc<Node>],
) -> Result<(Arc<QuicServer>, Vec<SocketAddr>)> {
    let agents = [
        "1.12.74.4:20004".parse()?,
        "[2402:4e00:c011:1700:8624:7e0:5c9a:2]:20004".parse()?,
    ];

    let factory = TraversalFactory::with(&agents[..]);

    let ip = bind.ip();
    let port = bind.port();
    let mut binds = Vec::new();

    if ip.is_unspecified() {
        for device_ip in factory.devices().keys() {
            binds.push(SocketAddr::new(device_ip.parse()?, port));
        }
    } else {
        binds.push(bind);
    }

    let mut builder = gm_quic::QuicServer::builder()
        .with_supported_versions([1u32]) // 支持QUIC版本1
        .without_client_cert_verifier() // 禁用证书验证
        .with_iface_factory(factory)
        .with_qlog(Arc::new(DefaultSeqLogger::new(PathBuf::from("/tmp/qlog"))))
        .with_parameters(create_server_params())
        .enable_sni();

    // 为每个服务器添加TLS证书
    for server in servers {
        let cert_path = if let Some(Value::Path(cert_path)) = server.get("ssl_certificate") {
            cert_path
        } else {
            unreachable!("Invalid ssl_certificate path");
        };

        let key_path = if let Some(Value::Path(key_path)) = server.get("ssl_certificate_key") {
            key_path
        } else {
            unreachable!("Invalid ssl_certificate_key path");
        };

        let server_name = if let Some(Value::StringVec(server_name)) = server.get("server_name") {
            server_name
        } else {
            unreachable!("Invalid server name");
        };

        let cert = std::fs::read(cert_path)?;
        let key = std::fs::read(key_path)?;
        for domain in server_name {
            builder = builder.add_host(domain, &*cert, &*key);
        }
    }

    info!("binds {:?}", binds);
    let server = builder
        .with_alpns([ALPN])
        .listen(&*binds)
        .inspect_err(|e| {
            error!("listen err {:?}", e);
        })?;
    Ok((server, binds))
}

/// 创建QUIC服务器参数配置
fn create_server_params() -> gm_quic::ServerParameters {
    let mut params = gm_quic::ServerParameters::default();

    params.set_initial_max_streams_bidi(100u32); // 双向流限制
    params.set_initial_max_streams_uni(100u32); // 单向流限制
    params.set_initial_max_data(1u32 << 30); // 连接总数据限制
    params.set_initial_max_stream_data_uni(1u32 << 30);
    params.set_initial_max_stream_data_bidi_local(1u32 << 30);
    params.set_initial_max_stream_data_bidi_remote(1u32 << 30);
    params.set_active_connection_id_limit(10u32);

    params
}

/// 处理客户端连接
async fn handle_connections(
    quic_server: Arc<QuicServer>,
    routers: Arc<HashMap<String, Arc<Node>>>,
) -> Result<()> {
    // 持续接受新连接
    while let Ok((conn, pathway)) = quic_server.accept().await {
        debug!(src_addr = ?pathway.local(), dst_addr = ?pathway.remote(), "accepted connection");

        // 将QUIC连接包装为H3 QUIC连接
        let h3_quic_conn = h3_shim::QuicConnection::new(conn).await;

        debug!("QUIC connection wrapped as H3 QUIC connection");

        // 建立H3连接
        let mut h3_conn = match h3::server::Connection::new(h3_quic_conn).await {
            Ok(conn) => {
                info!("[Handle Conn] H3 connection established");
                conn
            }
            Err(e) => {
                error!("[Handle Conn] Failed to establish H3 connection: {}", e);
                continue;
            }
        };

        debug!("[Handle Conn] H3 connection established");

        // 为每个连接创建异步任务
        tokio::spawn({
            let routers_clone = routers.clone();
            async move {
                while let Ok(Some((req, stream))) = h3_conn
                    .accept()
                    .await
                    .inspect_err(|e| error!("Connection acceptance error: {:?}", e))
                {
                    let routers = routers_clone.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_request(routers, req, stream).await {
                            error!("Request processing error: {}", e);
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
    servers: Arc<HashMap<String, Arc<Node>>>,
    req: Request<()>,
    stream: RequestStream<BidiStream<Bytes>, Bytes>,
) -> Result<()> {
    let host = req
        .uri()
        .authority()
        .ok_or(CustomError::MissingHost)?
        .host();

    // 查找匹配的路由规则
    // TODO 支持 泛域名匹配
    let server = servers
        .get(host)
        .ok_or_else(|| CustomError::RouterNotFound(host.to_string()))?;

    let locations = if let Some(Value::Nodes(locations)) = server.get("location") {
        locations
    } else {
        &Vec::new()
    };

    let (location, final_pattern) = match_location(locations, req.uri().path())
        .ok_or_else(|| CustomError::RouterNotFound(host.to_string()))?;

    let (mut sender, receiver) = stream.split();

    let location_value = if let Value::Pattern(_, map) = location.value() {
        map
    } else {
        unreachable!("Invalid location value");
    };

    match location_value {
        location_value if location_value.contains_key("proxy_pass") => {
            reverse::proxy::handle(location, req, receiver, sender).await?;
        }
        location_value if location_value.contains_key("root") => {
            reverse::file::root(location, req, sender).await?;
        }
        location_value if location_value.contains_key("alias") => {
            reverse::file::alias(location, final_pattern, req, sender).await?;
        }
        #[cfg(feature = "sshd")]
        location_value if location_value.contains_key("ssh_login") => {
            reverse::sshd::login(location, req, receiver, sender).await?;
        }
        _ => {
            let response = Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(())
                .expect("Failed to build response");

            sender.send_response(response).await?;
            sender.finish().await?;
        }
    }
    Ok(())
}

fn match_location<'l>(locations: &'l [Arc<Node>], path: &str) -> Option<(&'l Arc<Node>, String)> {
    debug!("all locations {:#?}, path: {:?}", locations, path);

    // 遍历所有location 匹配最高优先级的最长匹配
    let mut location_matched = None;
    let mut pattern_level = 0;
    let mut matched_len = 0;
    let mut final_pattern = String::new();

    for location in locations {
        let pattern = if let Value::Pattern(pattern, _) = location.value() {
            pattern
        } else {
            unreachable!("Invalid location pattern");
        };

        if pattern.priority() < pattern_level {
            continue;
        }

        if let Ok(Some(matched)) = pattern.try_match(path) {
            if matched.len() >= matched_len {
                location_matched = Some(location);
                pattern_level = pattern.priority();
                matched_len = matched.len();
                final_pattern = matched;
            }
        };
    }

    Some((location_matched?, final_pattern))
}

/// 构造错误响应
fn build_error_response() -> Response<()> {
    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .body(())
        .unwrap()
}
