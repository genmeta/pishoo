use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, SocketAddr},
    sync::Arc,
};

use bytes::Bytes;
use gm_quic::{BindUri, QuicListeners, handy::server_parameters};
use h3::server::RequestStream;
use h3_shim::BidiStream;
use http::{Request, Response, StatusCode};
use qdns::{HttpResolver, MdnsResolver, Publisher, Resolve};
use qtraversal::iface::{TraversalFactory, traversal_factory};
use tracing::{debug, error, info};

use crate::{
    error::{CustomError, Result},
    parse::{IPVersion, IfaceType, Listen, Node, Resolver, Value},
    reverse,
};

mod file;
mod proxy;
#[cfg(feature = "sshd")]
mod sshd;

type RouterMap = Arc<HashMap<String, Arc<Node>>>;
type ServerResolverList<'a> = Vec<(String, Vec<&'a Resolver>)>;

/// Start the QUIC proxy server
///
/// # Arguments
/// * `bind` - The listening address of the server
/// * `servers` - The list of server configurations
///
/// # Returns
/// * `Result<()>` - An empty result if successful, or an error if failed
pub async fn serve(
    servers: Vec<Arc<Node>>,
    mut stop_rx: Option<tokio::sync::broadcast::Receiver<()>>,
) -> Result<String> {
    let (routers, server_resolvers) = init_routers(&servers)?;
    let quic_listners = create_quic_server(&servers)?;

    let http_resovler = Arc::new(HttpResolver::new(qdns::HTTP_DNS_SERVER)?);
    let mdns_resovler = Arc::new(MdnsResolver::new(qdns::MDNS_SERVICE)?);
    let server_resolvers: HashMap<String, Vec<Arc<dyn Resolve + Send + Sync>>> = server_resolvers
        .into_iter()
        .map(|(server_name, resolvers)| {
            let server_resolvers: Vec<Arc<dyn Resolve + Send + Sync>> =
                if ["test.genmeta.net", "user.genmeta.net"]
                    .iter()
                    .any(|suffix| server_name.ends_with(suffix))
                {
                    vec![mdns_resovler.clone()]
                } else if resolvers.is_empty() {
                    vec![mdns_resovler.clone(), http_resovler.clone()]
                } else {
                    debug_assert!(!resolvers.is_empty());
                    let mut resolver_map: HashMap<String, Arc<dyn Resolve + Send + Sync>> =
                        HashMap::new();
                    // 提供默认 resovler
                    resolver_map.insert(http_resovler.server(), http_resovler.clone());
                    resolver_map.insert(mdns_resovler.server(), mdns_resovler.clone());
                    resolvers
                        .iter()
                        .map(|resolver| {
                            resolver_map
                                .entry(resolver.server_name())
                                .or_insert_with(|| (*resolver).into())
                                .clone()
                        })
                        .collect()
                };
            (server_name, server_resolvers)
        })
        .collect();

    // 启动 dns 上报
    let _publisher = Publisher::spawn(quic_listners.clone(), server_resolvers);

    // 主接受循环与停止信号并行监听
    tokio::select! {
        res = handle_connections(Arc::clone(&quic_server), routers) => {
            res?;
        }
        _ = async {
            if let Some(rx) = stop_rx.as_mut() {
                let _ = rx.recv().await;
            } else {
                futures::future::pending::<()>().await;
            }
        } => {
            quic_server.shutdown();
            info!("reverse::serve stop signal received, exiting accept loop");
        }
    }

    Ok("Server exited".to_string())
}

/// 初始化路由器，根据服务器配置创建路由表
fn init_routers(servers: &'_ [Arc<Node>]) -> Result<(RouterMap, ServerResolverList<'_>)> {
    let mut routers = HashMap::new();
    let mut resolvers = vec![];

    for server in servers {
        let server_resolvers = match server.get("resolver") {
            Some(Value::Resolver(resolver)) => vec![resolver],
            _ => vec![], // 默认使用空 resolver
        };

        let server_name = match server.get("server_name") {
            Some(Value::StringVec(names)) => names.clone(),
            _ => unreachable!("Invalid server name"),
        };

        for mut domain in server_name {
            if domain.ends_with('~') {
                domain = domain.replace('~', ".genmeta.net");
            }
            resolvers.push((domain.clone(), server_resolvers.clone()));
            routers.insert(domain, Arc::clone(server));
        }
    }

    Ok((Arc::new(routers), resolvers))
}

/// 创建QUIC服务器实例
fn create_quic_server(servers: &[Arc<Node>]) -> Result<Arc<QuicListeners>> {
    let agents: [SocketAddr; 2] = [
        "1.12.74.4:20004".parse()?,
        "[2402:4e00:c011:1700:8624:7e0:5c9a:2]:20004".parse()?,
    ];

    let factory = traversal_factory(&agents[..]);

    let mut server_binds = HashMap::new();

    for server in servers {
        let Some(Value::Listen(server_ifaces)) = server.get("listen") else {
            unreachable!("Invalid listen address");
        };

        let Some(Value::StringVec(server_names)) = server.get("server_name").cloned() else {
            unreachable!("Invalid server name");
        };

        let server_ifaces: HashSet<_> = server_ifaces.iter().cloned().collect();

        for mut domain in server_names {
            if domain.ends_with('~') {
                domain = domain.replace('~', ".genmeta.net");
            }
            server_binds.insert(domain, server_ifaces.clone());
        }
    }

    let srever_binds = server_binds
        .into_iter()
        .map(|(server_name, server_listen)| {
            let binds = server_listen
                .iter()
                .flat_map(|iface| resolve_binds(&factory, iface))
                .map(BindUri::from)
                .collect::<HashSet<_>>();
            (server_name, binds)
        })
        .collect::<HashMap<_, _>>();

    // collect & dedup
    let binds: HashSet<_> = srever_binds.values().flatten().cloned().collect();
    let factory = traversal_factory(&agents[..]);
    let builder = gm_quic::QuicListeners::builder().map_err(|e| {
        error!("Failed to create QUIC listener builder: {}", e);
        CustomError::LocalhostNotInitialized
    })?;

    #[cfg(feature = "qlog")]
    {
        use std::path::PathBuf;

        use qevent::telemetry::handy::DefaultSeqLogger;
        builder = builder.with_qlog(Arc::new(DefaultSeqLogger::new(PathBuf::from("/tmp/qlog"))));
    }
    let listener = builder
        .with_iface_factory(factory.as_ref().clone())
        .with_parameters(server_parameters())
        .without_client_cert_verifier()
        .listen(1000);

    // 为每个服务器添加TLS证书
    for server in servers {
        let Some(Value::Path(cert_path)) = server.get("ssl_certificate") else {
            unreachable!("Invalid ssl_certificate path");
        };

        let Some(Value::Path(key_path)) = server.get("ssl_certificate_key") else {
            unreachable!("Invalid ssl_certificate_key path");
        };

        let Some(Value::StringVec(server_name)) = server.get("server_name").cloned() else {
            unreachable!("Invalid server name");
        };

        let cert = std::fs::read(cert_path)?;
        let key = std::fs::read(key_path)?;
        for mut domain in server_name {
            if domain.ends_with('~') {
                domain = domain.replace('~', ".genmeta.net");
            }
            // builder = builder.add_host(domain, &*cert, &*key);
            let binds = server_binds.get(&domain).unwrap();
            info!("Adding server {} with binds {:?}", domain, binds);
            _ = listener.add_server(domain, &*cert, &*key, binds.clone(), None);
        }
    }

    info!("binds {:?}", binds);
    Ok(listener)
}

fn resolve_binds(factory: &TraversalFactory, iface: &Listen) -> Vec<String> {
    let mut binds = Vec::new();
    for (device_ip, device_name) in factory.devices() {
        let is_match = match (&iface.typ, device_ip) {
            (IfaceType::All, _) => true,
            (IfaceType::External, IpAddr::V4(ip)) => !ip.is_loopback(),
            (IfaceType::External, IpAddr::V6(ip)) => !ip.is_loopback(),
            (IfaceType::Internal, IpAddr::V4(ip)) => ip.is_loopback(),
            (IfaceType::Internal, IpAddr::V6(ip)) => ip.is_loopback(),
            (IfaceType::Exact(name), _) => factory.devices().get(device_ip) == Some(name),
        };
        let version_match = match device_ip {
            IpAddr::V4(_) => matches!(iface.version, IPVersion::V4 | IPVersion::Dual),
            IpAddr::V6(_) => matches!(iface.version, IPVersion::V6 | IPVersion::Dual),
        };
        if is_match && version_match {
            let family = match device_ip {
                IpAddr::V4(_) => "v4",
                IpAddr::V6(_) => "v6",
            };
            let bind_uri = format!("iface://{family}.{device_name}:5387");
            binds.push(bind_uri);
        }
    }
    binds
}

/// 处理客户端连接
async fn handle_connections(
    quic_server: Arc<QuicListeners>,
    routers: Arc<HashMap<String, Arc<Node>>>,
) -> Result<()> {
    // 持续接受新连接
    while let Ok((conn, _name, pathway, ..)) = quic_server.accept().await {
        debug!(src_addr = ?pathway.local(), dst_addr = ?pathway.remote(), "accepted connection");

        // 将QUIC连接包装为H3 QUIC连接
        let h3_quic_conn = h3_shim::QuicConnection::new(conn);

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

        debug!("RouterMap: {:?}", routers);

        // 为每个连接创建异步任务
        tokio::spawn({
            let routers_clone = routers.clone();
            async move {
                while let Ok(Some(req_resolver)) = h3_conn
                    .accept()
                    .await
                    .inspect_err(|e| error!("Connection acceptance error: {:?}", e))
                {
                    let routers = routers_clone.clone();
                    let handle_request = async move {
                        let (mut req, stream) = req_resolver.resolve_request().await?;
                        let addr = match pathway.remote() {
                            gm_quic::EndpointAddr::Socket(socket_endpoint_addr) => {
                                socket_endpoint_addr.addr()
                            }
                            gm_quic::EndpointAddr::Ble(_) => {
                                unreachable!("BLE endpoint not supported")
                            }
                        };
                        req.extensions_mut().insert(addr);
                        handle_request(routers, req, stream).await
                    };
                    tokio::spawn(async move {
                        if let Err(e) = handle_request.await {
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

    let Value::Pattern(_, location_value) = location.value() else {
        unreachable!("Invalid location value");
    };

    match location_value {
        location_value if location_value.contains_key("proxy_pass") => {
            reverse::proxy::handle(location, &final_pattern, req, receiver, sender).await?;
        }
        location_value if location_value.contains_key("root") => {
            reverse::file::root(location, req, sender).await?;
        }
        location_value if location_value.contains_key("alias") => {
            reverse::file::alias(location, &final_pattern, req, sender).await?;
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

fn match_location<'l: 's, 's>(
    locations: &'l [Arc<Node>],
    path: &'s str,
) -> Option<(&'l Arc<Node>, &'s str)> {
    debug!("all locations {:#?}, path: {:?}", locations, path);

    // 遍历所有location 匹配最高优先级的最长匹配
    let mut location_matched = None;
    let mut pattern_level = 0;
    let mut matched_len = 0;
    let mut final_pattern = "";

    for location in locations {
        let pattern = if let Value::Pattern(pattern, _) = location.value() {
            pattern
        } else {
            unreachable!("Invalid location pattern");
        };

        if pattern.priority() < pattern_level {
            continue;
        }

        if let Ok(Some(matched)) = pattern.try_match(path)
            && matched.len() >= matched_len
        {
            location_matched = Some(location);
            pattern_level = pattern.priority();
            matched_len = matched.len();
            final_pattern = matched;
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
