use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use bytes::Bytes;
use firewall_base::{
    action::RequestAction,
    expr::atomics::HttpRequest,
    matcher::{DomainRulesMatcher, LocationRulesMatcher},
};
use gm_quic::{
    prelude::{
        Connection, QuicListeners,
        handy::{ToCertificate, server_parameters},
    },
    qinterface::device::{Devices, Interface, InterfaceEvent, InterfacesMonitor},
};
use gmdns::resolver::{HTTP_DNS_SERVER, HttpResolver};
use h3::server::RequestStream;
use h3_shim::BidiStream;
use http::{HeaderValue, Request, Response, StatusCode};
use rustls::{SignatureScheme, server::WebPkiClientVerifier, sign::SigningKey};
use snafu::{OptionExt, Report, ResultExt};
use tokio::fs;
use tracing::{Instrument, debug, error, info, info_span};

use crate::{
    error::{Result, StreamSnafu, Whatever},
    parse::{DnsResolver, Listens, Node, Value},
    publisher::{Publisher, ServerConfig},
    reverse::{self, auth::load_key},
};

mod auth;
mod file;
pub(crate) mod log;
mod proxy;
#[cfg(feature = "sshd")]
mod sshd;

/*
 - PhysicalInterfaces:
    - 监听网络设备变化
    - 自动触发Interface的rebind
    - 发布InterfaceEvents供其他模块订阅网络变化，来添加/移除监听地址等

 - QuicListeners：
    - 初始化时
     - 根据listen配置，进行第一次绑定
    - 订阅Locations监听变化
     - 根据server的listen配置，响应变化（移除/添加bind地址）

 - DNS发布任务
    - 订阅Locations监听变化
     - 根据server的listen和resolver配置
    - 响应变化（移除/添加mDNS Resolver）
     - 进行重新发布

 - QuicClient：
    - 初始化时
     - 根据listen配置，进行第一次绑定
    - 订阅Locations监听变化
     - 根据client的listen配置，响应变化（移除/添加bind地址）
*/

type RouterMap = Arc<HashMap<String, Arc<Node>>>;
type ServerResolverList<'a> = Vec<(
    String,
    Vec<&'a DnsResolver>,
    Vec<&'a crate::parse::DnsPublisher>,
    u8,
    Option<(Arc<dyn SigningKey>, SignatureScheme)>,
)>;

/// Start the QUIC proxy server
///
/// # Arguments
/// * `bind` - The listening address of the server
/// * `servers` - The list of server configurations
///
/// # Returns
/// * `Result<()>` - An empty result if successful, or an error if failed
pub async fn serve(
    access_rules: (Arc<DomainRulesMatcher>, Arc<LocationRulesMatcher>),
    servers: Vec<Arc<Node>>,
) -> Result<()> {
    let monitor = Devices::global().monitor();
    let current_interfaces = monitor.interfaces();
    let (router, server_resolvers) = init_router(&servers)?;
    let (quic_listeners, server_listens) =
        create_quic_listeners(access_rules.0, current_interfaces, &servers).await?;

    let default_http_resolver = Arc::new(
        HttpResolver::new(HTTP_DNS_SERVER)
            .whatever_context::<_, Whatever>("Failed to create HTTP dns resolver")?,
    );

    let server_resolvers: HashMap<String, ServerConfig> = server_resolvers
        .into_iter()
        .map(
            |(server_name, _resolvers, publishers, id, key): (
                String,
                Vec<&DnsResolver>,
                Vec<&crate::parse::DnsPublisher>,
                u8,
                Option<(Arc<dyn SigningKey>, SignatureScheme)>,
            )| {
                let resolvers: Vec<Arc<dyn gmdns::resolver::Publisher + Send + Sync>> =
                    if ["test.genmeta.net", "user.genmeta.net"]
                        .iter()
                        .any(|suffix| server_name.ends_with(suffix))
                    {
                        vec![]
                    } else if publishers.is_empty() {
                        vec![default_http_resolver.clone()]
                    } else {
                        debug_assert!(!publishers.is_empty());
                        publishers
                            .iter()
                            .map(|p: &&crate::parse::DnsPublisher| p.create_publisher())
                            .collect()
                    };
                (
                    server_name,
                    ServerConfig {
                        resolvers,
                        server_id: id,
                        signing_key: key,
                    },
                )
            },
        )
        .collect();

    // 启动 dns 上报
    let _publisher = Publisher::spawn(quic_listeners.clone(), server_resolvers);
    let _guard = ShutdownListenersOnDrop(quic_listeners.clone());
    let _maintain_binding = tokio::spawn(maintain_binding(
        monitor,
        quic_listeners.clone(),
        server_listens,
    ));

    // 主接受循环
    handle_connections(quic_listeners, access_rules.1, router).await
}

/// 初始化路由器，根据服务器配置创建路由表
fn init_router(servers: &'_ [Arc<Node>]) -> Result<(RouterMap, ServerResolverList<'_>)> {
    let mut routers = HashMap::new();
    let mut resolvers = vec![];

    for server in servers {
        let server_resolvers = match server.get("resolver") {
            Some(Value::DnsResolver(resolver)) => vec![resolver],
            _ => vec![], // 默认使用空 resolver
        };

        let server_publishers = match server.get("publisher") {
            Some(Value::DnsPublisher(publisher)) => vec![publisher],
            _ => vec![], // 默认使用空 publisher
        };

        let server_name = match server.get("server_name") {
            Some(Value::ServerName(names)) => names.clone(),
            _ => unreachable!("Invalid server name"),
        };

        let server_id = match server.get("server_id") {
            Some(Value::ServerId(id)) => *id,
            _ => 0, // 默认为 0
        };

        let key_path = match server.get("ssl_certificate_key") {
            Some(Value::Path(path)) => path,
            _ => unreachable!("Invalid ssl_certificate_key path"),
        };
        let key_pair = load_key(key_path).ok();

        for server_name_struct in server_name {
            let mut domain = server_name_struct.name;
            if domain.ends_with('~') {
                domain = domain.replace('~', ".genmeta.net");
            }
            resolvers.push((
                domain.clone(),
                server_resolvers.clone(),
                server_publishers.clone(),
                server_id,
                key_pair.clone(),
            ));
            routers.insert(domain, Arc::clone(server));
        }
    }

    Ok((Arc::new(routers), resolvers))
}

/// 创建QUIC服务器实例
async fn create_quic_listeners(
    domain_access_rules: Arc<DomainRulesMatcher>,
    current_interfaces: &HashMap<String, Interface>,
    servers: &[Arc<Node>],
) -> Result<(Arc<QuicListeners>, HashMap<String, Vec<Listens>>)> {
    let mut server_listens = HashMap::new();

    for server in servers {
        let Some(Value::Listen(listens)) = server.get("listen") else {
            unreachable!("Invalid listen address");
        };

        let Some(Value::ServerName(server_names)) = server.get("server_name").cloned() else {
            unreachable!("Invalid server name");
        };

        for server_name_struct in server_names {
            let mut server_name = server_name_struct.name;
            if server_name.ends_with('~') {
                server_name = server_name.replace('~', ".genmeta.net");
            }
            server_listens.insert(server_name, listens.clone());
        }
    }

    let server_bind_uris = server_listens
        .iter()
        .map(|(server_name, server_listen)| {
            let bind_uris = server_listen
                .iter()
                .flat_map(|listens| listens.resolve(current_interfaces.keys().map(|s| s.as_str())))
                .collect::<HashSet<_>>();
            (server_name, bind_uris)
        })
        .collect::<HashMap<_, _>>();

    // collect & dedup
    let initial_bind_uris: HashSet<_> = server_bind_uris.values().flatten().cloned().collect();
    debug!(?initial_bind_uris, "Binds");

    #[allow(unused_mut)]
    let mut builder = QuicListeners::builder();

    #[cfg(feature = "qlog")]
    {
        use std::path::PathBuf;

        use qevent::telemetry::handy::LegacySeqLogger;
        builder = builder.with_qlog(Arc::new(LegacySeqLogger::new(PathBuf::from("/tmp/qlog"))));
    }

    let mut roots = rustls::RootCertStore::empty();
    roots.add_parsable_certificates(include_bytes!("../../root.crt").to_certificate());

    let tls_client_cert_verifier = WebPkiClientVerifier::builder(Arc::new(roots))
        // 允许client不带证书
        .allow_unauthenticated()
        .build()
        .unwrap();

    let listeners = builder
        .with_parameters(server_parameters())
        .with_client_cert_verifier(tls_client_cert_verifier)
        .with_client_auther(auth::ClientAuther::from(domain_access_rules))
        .listen(1024)
        .whatever_context::<_, Whatever>("Failed to listen quic")?;

    // 为每个服务器添加TLS证书
    for server in servers {
        let Some(Value::Path(cert_path)) = server.get("ssl_certificate") else {
            unreachable!("Invalid ssl_certificate path");
        };

        let Some(Value::Path(key_path)) = server.get("ssl_certificate_key") else {
            unreachable!("Invalid ssl_certificate_key path");
        };

        let Some(Value::ServerName(server_names)) = server.get("server_name").cloned() else {
            unreachable!("Invalid server name");
        };

        let cert = fs::read(cert_path)
            .await
            .whatever_context::<_, Whatever>(format!(
                "Failed to read certificate file `{}`",
                cert_path.display()
            ))?;
        let key = fs::read(key_path)
            .await
            .whatever_context::<_, Whatever>(format!(
                "Failed to read private key file `{}`",
                key_path.display()
            ))?;
        for server_name_struct in server_names {
            let mut server_name = server_name_struct.name;
            if server_name.ends_with('~') {
                server_name = server_name.replace('~', ".genmeta.net");
            }
            let bind_uris = server_bind_uris.get(&server_name).unwrap();
            debug!(server_name, ?bind_uris, "Adding server");
            listeners
                .add_server(
                    &server_name,
                    cert.as_slice(),
                    key.as_slice(),
                    bind_uris.clone(),
                    None,
                )
                .await
                .whatever_context::<_, Whatever>("Failed to initial quic listeners")?;
        }
    }

    Ok((listeners, server_listens))
}

#[tracing::instrument(name = "maintain_binding", skip_all)]
async fn maintain_binding(
    mut monitor: InterfacesMonitor,
    quic_listeners: Arc<QuicListeners>,
    server_listens: HashMap<String, Vec<Listens>>,
) {
    while let Some((_currnet_interfaces, event)) = monitor.update().await {
        //tracing::debug!(target: "listen", ?event, "Interface event received");
        match event.as_ref() {
            InterfaceEvent::Added { device, .. } => {
                let mut main_bind_uris = HashSet::new();

                // 启动主Quic监听的接口绑定
                for (server, listens) in &server_listens {
                    let bind_uris = listens
                        .iter()
                        .flat_map(|listens| listens.resolve([device.as_str()]))
                        .collect::<HashSet<_>>();
                    if bind_uris.is_empty() {
                        continue;
                    }
                    debug!(target: "listen", server, ?bind_uris, "Add interfaces to server binding");
                    let Some(server) = quic_listeners.get_server(server) else {
                        unreachable!()
                    };
                    for bind_uri in bind_uris {
                        // Server will bind the interface using its configured IO factory.
                        server.bind([bind_uri.clone()]).await;
                        main_bind_uris.insert(bind_uri);
                    }
                }
            }
            InterfaceEvent::Removed { device, .. } => {
                for (server, listens) in &server_listens {
                    let bind_uris = listens
                        .iter()
                        .flat_map(|listens| listens.resolve([device.as_str()]))
                        .collect::<HashSet<_>>();
                    if bind_uris.is_empty() {
                        continue;
                    }
                    debug!(target: "listen", server, ?bind_uris, "Remove those interface from server binding");
                    let Some(server) = quic_listeners.get_server(server) else {
                        unreachable!()
                    };
                    for bind_uri in bind_uris {
                        _ = server.remove_iface(&bind_uri);
                    }
                }
            }
            InterfaceEvent::Changed { .. } => { /* Ignore changes */ }
        }
    }
}

/// 处理客户端连接
async fn handle_connections(
    quic_server: Arc<QuicListeners>,
    access_rules: Arc<LocationRulesMatcher>,
    router: Arc<HashMap<String, Arc<Node>>>,
) -> Result<()> {
    let handle_connection = async |conn: Arc<Connection>, server_name: String| {
        info!(target: "connect", "Accepted connection");
        // 将QUIC连接包装为H3 QUIC连接
        let h3_quic_conn = h3_shim::QuicConnection::new(conn.clone());

        debug!(target: "connect", "QUIC connection wrapped as H3 QUIC connection");

        // 建立H3连接
        let mut h3_conn = h3::server::Connection::<_, Bytes>::new(h3_quic_conn)
            .await
            .whatever_context::<_, Whatever>("Failed to establish H3 connection")?;

        debug!(target: "connect", "H3 connection established");
        debug!(target: "connect", "RouterMap: {:?}", router);

        let router = router.clone();
        let access_rules = access_rules.clone();

        let handle_request = Arc::new(async move |request: Request<()>, stream| {
            let span = info_span!("handle_request", uri=%request.uri());
            let handle_request = handle_request(
                server_name.clone(),
                router.clone(),
                access_rules.clone(),
                conn.clone(),
                request,
                stream,
            )
            .await;

            async move {
                info!(target: "request", "Resolved new request");

                if let Err(handle_request_error) = handle_request {
                    error!(
                        target: "request", "Failed to handle resolved request: {}",
                        Report::from_error(handle_request_error)
                    );
                }
            }
            .instrument(span)
            .await
        });

        // 为每个连接创建异步任务
        let accept_requests = async move {
            while let Ok(Some(req_resolver)) = h3_conn.accept().await.inspect_err(|error| {
                error!(
                    target: "connect", "Failed to accept more request: {}",
                    Report::from_error(error.clone())
                )
            }) {
                let handle_request = handle_request.clone();
                let handle_and_resolve_request = async move {
                    match req_resolver.resolve_request().await {
                        Ok((request, stream)) => {
                            handle_request(request, stream).await;
                        }
                        Err(e) => error!(
                            target: "request", "Failed to resolve request: {}",
                            Report::from_error(e)
                        ),
                    }
                };
                tokio::spawn(handle_and_resolve_request.in_current_span());
            }
        };
        Result::<_, Whatever>::Ok(tokio::spawn(accept_requests.in_current_span()))
    };

    // 持续接受新连接
    while let Ok((conn, server_name, pathway, link)) = quic_server.accept().await {
        let span = info_span!(
            "handle_connection",
            %server_name,
            %pathway,
            %link
        );
        async move {
            if let Err(error) = handle_connection(conn.into(), server_name).await {
                error!(
                    target: "connect", "Failed to handle connection: {}",
                    Report::from_error(error)
                );
            }
        }
        .instrument(span)
        .await;
    }
    Ok(())
}

/// 处理单个HTTP请求
async fn handle_request(
    server_name: String,
    servers: Arc<HashMap<String, Arc<Node>>>,
    access_rules: Arc<LocationRulesMatcher>,
    conn: Arc<Connection>,
    req: Request<()>,
    stream: RequestStream<BidiStream<Bytes>, Bytes>,
) -> Result<()> {
    tracing::debug!(target: "request", ?req);
    // 查找匹配的路由规则
    // TODO 支持 泛域名匹配
    let server = servers
        .get(&server_name)
        .whatever_context::<_, Whatever>(format!(
            "No matched server for request's host `{server_name}`",
        ))?;

    let locations = if let Some(Value::Nodes(locations)) = server.get("location") {
        locations
    } else {
        &Vec::new()
    };

    let (location, final_pattern) = match_location(locations, req.uri().path())
        .whatever_context::<_, Whatever>(format!(
            "No matched location for path `{}` in server `{}`",
            req.uri().path(),
            server_name
        ))?;
    let final_pattern = final_pattern.to_string();

    let client_name = conn
        .remote_agent()
        .await
        .ok()
        .flatten()
        .map(|agent| agent.name().to_string());
    let http_request = HttpRequest::new(client_name.as_deref(), &req);

    #[allow(unused_variables)]
    let (firewall_matched_domain, firewall_matched_location, firewall_action) =
        match access_rules.match_rule(&server_name, req.uri().path(), &http_request) {
            Ok((domain, location, action)) => (Some(domain), Some(location), action),
            Err(..) => (None, None, RequestAction::Allow),
        };

    let (mut sender, recver) = stream.split();

    let client_name = match &client_name {
        None => "<anonymous>",
        Some(name) => name,
    };

    if firewall_action == RequestAction::Deny {
        let response = Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(())
            .expect("Failed to build response");

        info!(target: "request", "Firewall rules deny request from client `{client_name} to server `{server_name} with uri `{}`", req.uri());
        sender.send_response(response).await.context(StreamSnafu)?;
        sender.finish().await.context(StreamSnafu)?;
        return Ok(());
    }

    let Value::Pattern(_, location_values) = location.value() else {
        unreachable!("Invalid location value");
    };

    let (mut parts, body) = req.into_parts();
    parts
        .headers
        .insert("ClientName", HeaderValue::from_str(client_name).unwrap());
    let req = Request::from_parts(parts, body);

    match location_values {
        location_value if location_value.contains_key("proxy_pass") => {
            reverse::proxy::handle(location, req, recver, sender).await?;
        }
        location_value if location_value.contains_key("root") => {
            reverse::file::root(location, req, sender).await?;
        }
        location_value if location_value.contains_key("alias") => {
            reverse::file::alias(location, &final_pattern, req, sender).await?;
        }
        #[cfg(feature = "sshd")]
        location_value if location_value.contains_key("ssh_login") => {
            let cn = client_name.to_string();
            let rule_set = firewall_matched_location;
            reverse::sshd::serve(location, final_pattern, rule_set, req, cn, recver, sender)
                .await?;
        }
        _ => {
            let response = Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(())
                .expect("Failed to build response");

            sender.send_response(response).await.context(StreamSnafu)?;
            sender.finish().await.context(StreamSnafu)?;
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

fn build_response(status: StatusCode) -> Response<()> {
    Response::builder()
        .status(status)
        .body(())
        .expect("Failed to build response")
}

/// 构造错误响应
fn build_error_response() -> Response<()> {
    build_response(StatusCode::INTERNAL_SERVER_ERROR)
}

struct ShutdownListenersOnDrop(Arc<QuicListeners>);

impl Drop for ShutdownListenersOnDrop {
    fn drop(&mut self) {
        self.0.shutdown();
    }
}
