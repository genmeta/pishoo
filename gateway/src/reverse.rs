use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use firewall_base::{
    action::RequestAction,
    expr::atomics::HttpRequest,
    matcher::{DomainRulesMatcher, LocationRulesMatcher},
};
use gm_quic::{
    prelude::{QuicListeners, handy::server_parameters},
    qinterface::device::{Devices, Interface, InterfaceEvent, InterfacesMonitor},
};
use h3x::{
    connection::Connection as H3Connection,
    dhttp::settings::Settings,
    message::stream::{ReadStream, WriteStream},
};
use http::{HeaderValue, Request, Response, StatusCode};
use rustls::server::WebPkiClientVerifier;
use snafu::{OptionExt, Report, ResultExt};
use tokio::fs;
use tracing::{Instrument, debug, error, info, info_span, warn};

use crate::{
    dns::{Publisher, build_publish_configs, build_query_resolver_chain},
    error::{Result, StreamSnafu, Whatever},
    parse::{Listens, Node, Value},
    reverse::{self},
    stun::{STUN_DOMAIN, StunNodeConfig, StunServerManager},
};

mod auth;
mod file;
pub(crate) mod gzip;
pub(crate) mod log;
mod proxy;
#[cfg(feature = "sshd")]
mod sshd;
mod upstream_tls;

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

#[derive(Debug, Clone, Copy)]
pub enum MissingRulePolicy {
    Allow,
    Deny,
}

#[derive(Clone)]
struct RequestContext {
    server_name: String,
    servers: Arc<HashMap<String, Arc<Node>>>,
    access_rules: Arc<LocationRulesMatcher>,
    missing_rule_policy: MissingRulePolicy,
}

pub fn build_router_for_worker(servers: &[Arc<Node>]) -> Arc<HashMap<String, Arc<Node>>> {
    build_router(servers)
}

pub async fn handle_single_connection_for_worker(
    conn: impl h3x::quic::Connection + 'static,
    server_name: String,
    h3_settings: Arc<Settings>,
    router: Arc<HashMap<String, Arc<Node>>>,
    access_rules: Arc<LocationRulesMatcher>,
    missing_rule_policy: MissingRulePolicy,
) -> Result<()> {
    handle_single_connection(
        conn,
        server_name,
        h3_settings,
        router,
        access_rules,
        missing_rule_policy,
    )
    .await
}

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
    // 从第一个声明了 STUN 的 server 中提取运行时 STUN 配置
    let stun_config = servers.iter().find_map(StunNodeConfig::from_server_node);

    let monitor = Devices::global().monitor();
    let current_interfaces = monitor.interfaces();
    let router = build_router(&servers);
    let mut publish_configs = build_publish_configs(&servers)?;

    // stun.genmeta.net 由 StunServerManager 全权负责（包括启动和 DNS 上报），
    // 从 publish_configs 中取出其 PublishConfig，不再交给 Publisher
    let stun_publish_config = publish_configs.remove(STUN_DOMAIN);

    let (quic_listeners, server_listens) =
        create_quic_listeners(access_rules.0, current_interfaces, &servers).await?;

    info!("dns resolvers initialized");

    // 启动 dns 上报（不含 stun.genmeta.net）
    let _publisher = Publisher::spawn(quic_listeners.clone(), publish_configs);
    let _stun_manager = match (stun_config, stun_publish_config) {
        (Some(config), Some(publish_config)) => Some(StunServerManager::spawn(
            quic_listeners.clone(),
            publish_config,
            config,
        )),
        (Some(_), None) => {
            warn!("stun configured but no dns publisher for {STUN_DOMAIN}, stun server manager disabled");
            None
        }
        _ => None,
    };
    let _guard = ShutdownListenersOnDrop(quic_listeners.clone());
    let _maintain_binding = tokio::spawn(maintain_binding(
        monitor,
        quic_listeners.clone(),
        server_listens,
    ));

    // 主接受循环
    handle_connections(quic_listeners, access_rules.1, router).await
}

/// 根据服务器配置创建路由表
fn build_router(servers: &[Arc<Node>]) -> RouterMap {
    let mut routers = HashMap::new();

    for server in servers {
        let server_names = match server.get("server_name") {
            Some(Value::ServerName(names)) => names.clone(),
            _ => unreachable!("Invalid server name"),
        };

        for server_name in server_names {
            let domain = match server_name.name.strip_suffix('~') {
                Some(prefix) => format!("{prefix}.genmeta.net"),
                None => server_name.name.clone(),
            };
            routers.insert(domain, Arc::clone(server));
        }
    }

    Arc::new(routers)
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
            let server_name = match server_name_struct.name.strip_suffix('~') {
                Some(prefix) => format!("{prefix}.genmeta.net"),
                None => server_name_struct.name,
            };
            server_listens.insert(server_name, listens.clone());
        }
    }

    let server_bind_uris = server_listens
        .iter()
        .map(|(server_name, server_listen)| {
            let bind_uris = server_listen
                .iter()
                .flat_map(|listens| listens.resolve(current_interfaces.keys().map(|s| s.as_str())))
                .filter(|uri| uri.resolve().is_ok())
                .collect::<HashSet<_>>();
            (server_name, bind_uris)
        })
        .collect::<HashMap<_, _>>();

    // collect & dedup
    let initial_bind_uris: HashSet<_> = server_bind_uris.values().flatten().cloned().collect();
    debug!(?initial_bind_uris, "resolved initial bind uris");

    #[allow(unused_mut)]
    let mut builder =
        QuicListeners::builder().with_resolver(Arc::new(build_query_resolver_chain(servers)));

    builder = builder.with_stun("stun.genmeta.net");

    #[cfg(feature = "qlog")]
    {
        use std::path::PathBuf;

        use qevent::telemetry::handy::LegacySeqLogger;
        builder = builder.with_qlog(Arc::new(LegacySeqLogger::new(PathBuf::from("/tmp/qlog"))));
    }

    let roots = crate::common::root_cert();

    let tls_client_cert_verifier = WebPkiClientVerifier::builder(roots)
        // 允许client不带证书
        .allow_unauthenticated()
        .build()
        .whatever_context::<_, Whatever>("Failed to build TLS client cert verifier")?;

    let listeners = builder
        .with_parameters(server_parameters())
        .with_client_cert_verifier(tls_client_cert_verifier)
        .with_alpns([b"h3".as_slice()])
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
            let server_name = match server_name_struct.name.strip_suffix('~') {
                Some(prefix) => format!("{prefix}.genmeta.net"),
                None => server_name_struct.name,
            };
            let bind_uris = server_bind_uris
                .get(&server_name)
                .whatever_context::<_, Whatever>(format!(
                    "No bind URIs found for server `{server_name}`"
                ))?;
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

                // 启动主Quic监听的接口绑定（仅绑定能解析出IP的接口）
                for (server, listens) in &server_listens {
                    let bind_uris = listens
                        .iter()
                        .flat_map(|listens| listens.resolve([device.as_str()]))
                        .filter(|uri| uri.resolve().is_ok())
                        .collect::<HashSet<_>>();
                    if bind_uris.is_empty() {
                        continue;
                    }
                    debug!(server, ?bind_uris, "adding interfaces to server binding");
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
                    debug!(server, ?bind_uris, "removing interfaces from server binding");
                    let Some(server) = quic_listeners.get_server(server) else {
                        unreachable!()
                    };
                    for bind_uri in bind_uris {
                        _ = server.remove_iface(&bind_uri);
                    }
                }
            }
            InterfaceEvent::Changed { device, .. } => {
                // 设备信息变化时（如获得/更换IP），尝试绑定新可用的接口
                for (server_name, listens) in &server_listens {
                    let bind_uris = listens
                        .iter()
                        .flat_map(|listens| listens.resolve([device.as_str()]))
                        .filter(|uri| uri.resolve().is_ok())
                        .collect::<HashSet<_>>();
                    if bind_uris.is_empty() {
                        continue;
                    }
                    let Some(server) = quic_listeners.get_server(server_name) else {
                        continue;
                    };
                    for bind_uri in bind_uris {
                        if server.get_iface(&bind_uri).is_none() {
                            debug!(server_name, %bind_uri, "interface changed, binding new address");
                            server.bind([bind_uri]).await;
                        }
                    }
                }
            }
        }
    }
}

/// 处理客户端连接
async fn handle_connections(
    quic_server: Arc<QuicListeners>,
    access_rules: Arc<LocationRulesMatcher>,
    router: Arc<HashMap<String, Arc<Node>>>,
) -> Result<()> {
    let h3_settings = Arc::new(Settings::default());

    // 持续接受新连接
    while let Ok((conn, server_name, pathway, link)) = quic_server.accept().await {
        let span = info_span!(
            "handle_connection",
            %server_name,
            %pathway,
            %link
        );
        let h3_settings = h3_settings.clone();
        let router = router.clone();
        let access_rules = access_rules.clone();
        tokio::spawn(
            async move {
                if let Err(error) =
                    handle_single_connection(
                        conn,
                        server_name,
                        h3_settings,
                        router,
                        access_rules,
                        MissingRulePolicy::Allow,
                    )
                        .await
                {
                    error!(
                        error = %Report::from_error(error),
                        "failed to handle connection"
                    );
                }
            }
            .instrument(span),
        );
    }
    Ok(())
}

/// 处理单个 QUIC 连接的 H3 握手和请求接受
async fn handle_single_connection(
    conn: impl h3x::quic::Connection + 'static,
    server_name: String,
    h3_settings: Arc<Settings>,
    router: Arc<HashMap<String, Arc<Node>>>,
    access_rules: Arc<LocationRulesMatcher>,
    missing_rule_policy: MissingRulePolicy,
) -> Result<()> {
    info!("accepted connection");

    // 建立H3连接
    let h3_conn = Arc::new(
        H3Connection::new(h3_settings, conn)
            .await
            .whatever_context::<_, Whatever>("Failed to establish H3 connection")?,
    );

    debug!("h3 connection established");
    debug!(router = ?router, "router map");

    let h3_conn_for_accept = h3_conn.clone();
    let handle_request_fn = Arc::new(
        async move |request: Request<()>, recver: ReadStream, sender: WriteStream| {
            let span = info_span!("handle_request", uri=%request.uri());
            let handle_result = handle_request(
                RequestContext {
                    server_name: server_name.clone(),
                    servers: router.clone(),
                    access_rules: access_rules.clone(),
                    missing_rule_policy,
                },
                h3_conn.clone(),
                request,
                recver,
                sender,
            )
            .await;

            async move {
                info!("resolved new request");

                if let Err(handle_request_error) = handle_result {
                    error!(
                        error = %Report::from_error(handle_request_error),
                        "failed to handle resolved request"
                    );
                }
            }
            .instrument(span)
            .await
        },
    );

    // 为每个连接创建异步任务
    let accept_requests = async move {
        loop {
            match h3_conn_for_accept.accept_message_stream().await {
                Ok((mut read_stream, write_stream)) => {
                    let handle_request_fn = handle_request_fn.clone();
                    let task = async move {
                        match read_stream.read_hyper_request_parts().await {
                            Ok(parts) => {
                                let request = Request::from_parts(parts, ());
                                handle_request_fn(request, read_stream, write_stream).await;
                            }
                            Err(error) => error!(
                                error = %Report::from_error(error),
                                "failed to read request"
                            ),
                        }
                    };
                    tokio::spawn(task.in_current_span());
                }
                Err(e) => {
                    error!(
                        error = %Report::from_error(e),
                        "failed to accept another request"
                    );
                    break;
                }
            }
        }
    };
    tokio::spawn(accept_requests.in_current_span());
    Ok(())
}

/// 处理单个HTTP请求
async fn handle_request(
    context: RequestContext,
    conn: Arc<H3Connection<impl h3x::quic::Connection + 'static>>,
    req: Request<()>,
    recver: ReadStream,
    sender: WriteStream,
) -> Result<()> {
    let RequestContext {
        server_name,
        servers,
        access_rules,
        missing_rule_policy,
    } = context;
    tracing::debug!(request = ?req, "received request");
    // 查找匹配的路由规则
    // TODO 支持 泛域名匹配
    let server = servers
        .get(&server_name)
        .whatever_context::<_, Whatever>(format!(
            "No matched server for request's host `{server_name}`",
        ))?;

    let locations = if let Some(Value::Nodes(locations)) = server.get("location") {
        locations.as_slice()
    } else {
        &[]
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
            Err(error) => {
                let action = action_on_missing_rule(missing_rule_policy);
                warn!(
                    %server_name,
                    path = %req.uri().path(),
                    ?missing_rule_policy,
                    %error,
                    "firewall rule matching failed"
                );
                (None, None, action)
            }
        };

    let client_name = match &client_name {
        None => "<anonymous>",
        Some(name) => name,
    };

    if firewall_action == RequestAction::Deny {
        info!(
            client_name,
            %server_name,
            uri = %req.uri(),
            "firewall rules denied request"
        );
        send_status_and_close(sender, StatusCode::FORBIDDEN).await?;
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
            send_status_and_close(sender, StatusCode::NOT_FOUND).await?;
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

fn action_on_missing_rule(policy: MissingRulePolicy) -> RequestAction {
    match policy {
        MissingRulePolicy::Allow => RequestAction::Allow,
        MissingRulePolicy::Deny => RequestAction::Deny,
    }
}

/// 发送状态码响应并关闭流
async fn send_status_and_close(mut sender: WriteStream, status: StatusCode) -> Result<()> {
    let (parts, _) = build_response(status).into_parts();
    sender
        .send_hyper_response_parts(parts)
        .await
        .context(StreamSnafu)?;
    sender.close().await.context(StreamSnafu)?;
    Ok(())
}

struct ShutdownListenersOnDrop(Arc<QuicListeners>);

impl Drop for ShutdownListenersOnDrop {
    fn drop(&mut self) {
        self.0.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_rule_policy_deny_is_fail_closed() {
        assert_eq!(action_on_missing_rule(MissingRulePolicy::Deny), RequestAction::Deny);
    }
}
