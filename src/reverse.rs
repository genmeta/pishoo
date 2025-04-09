use std::{collections::HashMap, net::SocketAddr, sync::Arc};

use bytes::Bytes;
use gm_quic::{QuicServer, prelude::handy::Usc};
use h3::server::RequestStream;
use h3_shim::BidiStream;
use http::{Request, Response, StatusCode};
use tracing::{debug, error, info, warn};

use crate::{
    dns::Dns,
    error::{CustomError, Result},
    localhost::ArcLocalHost,
    parse::{Node, Value},
    reverse,
};

mod file;
mod proxy;
#[cfg(feature = "sshd")]
mod sshd;

const ALPN: &[u8] = b"h3"; // 应用层协议协商标识
const MAX_STREAMS: u64 = 100; // 最大双向/单向流数量
const MAX_DATA: u32 = 1 << 30; // 最大数据限制 (1MB)

/// Start the QUIC proxy server
///
/// # Arguments
/// * `bind` - The listening address of the server
/// * `servers` - The list of server configurations
///
/// # Returns
/// * `Result<()>` - An empty result if successful, or an error if failed
pub async fn serve(bind: SocketAddr, servers: Vec<Arc<Node>>) -> Result<String> {
    let localhost = ArcLocalHost::new(bind.port());
    localhost.init_network().await;

    let routers = init_routers(&servers, localhost.clone())?;
    let quic_server = create_quic_server(localhost.clone(), &servers)?;

    handle_connections(quic_server, localhost, routers).await?;
    Ok("Server exited".to_string())
}

/// 初始化路由器，根据服务器配置创建路由表
fn init_routers(
    servers: &[Arc<Node>],
    localhost: ArcLocalHost,
) -> Result<Arc<HashMap<String, Arc<Node>>>> {
    let mut routers = HashMap::new();
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

        let resolver = Dns::new(resolver);
        resolver.spwan_publish(server_name.clone(), localhost.clone());

        for name in server_name {
            routers.insert(name.to_string(), server.clone());
        }
    }

    Ok(Arc::new(routers))
}

/// 创建QUIC服务器实例
fn create_quic_server(localhost: ArcLocalHost, servers: &[Arc<Node>]) -> Result<Arc<QuicServer>> {
    let params = create_server_params();
    let local_host = localhost.clone();
    let mut builder = QuicServer::builder()
        .with_supported_versions([1u32]) // 支持QUIC版本1
        .without_cert_verifier() // 禁用证书验证
        .with_iface_binder(move |addr| {
            if let Some(iface) = local_host.iface(addr) {
                debug!("bind iface {}", addr);
                Ok(Arc::new(Usc::new(iface)?))
            } else {
                warn!("bind iface error");
                Ok(Arc::new(Usc::bind(addr)?))
            }
        })
        .with_parameters(params)
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
    routers: Arc<HashMap<String, Arc<Node>>>,
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
                error!("[Handle Conn] Failed to establish H3 connection: {}", e);
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

    let (sender, receiver) = stream.split();

    let location_value = if let Value::Pattern(_, map) = location.value() {
        map
    } else {
        unreachable!("Invalid location value");
    };

    if location_value.contains_key("proxy_pass") {
        reverse::proxy::handle(location, req, receiver, sender).await?;
    } else if location_value.contains_key("root") {
        reverse::file::root(location, req, sender).await?;
    } else if location_value.contains_key("alias") {
        reverse::file::alias(location, final_pattern, req, sender).await?;
    } else if location_value.contains_key("ssh_login") {
        #[cfg(feature = "sshd")]
        reverse::sshd::login(location, req, receiver, sender).await?;
    }

    Ok(())
}

fn match_location<'l>(locations: &'l [Arc<Node>], path: &str) -> Option<(&'l Arc<Node>, String)> {
    info!("all locations {:#?}, path: {:?}", locations, path);

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
