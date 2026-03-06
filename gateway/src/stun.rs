use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use gm_quic::{
    prelude::{BindUri, BoundAddr, IO, QuicListeners},
    qbase::net::Family,
    qinterface::{
        BindInterface, WeakInterface,
        bind_uri::BindUriScheme,
        component::location::Locations,
        io::{ProductIO, handy::DEFAULT_IO_FACTORY},
    },
    qtraversal::{
        nat::{
            client::{NatType, StunClientsComponent},
            router::{StunRouter, StunRouterComponent},
            server::{StunServer, StunServerConfig},
        },
        route::ReceiveAndDeliverPacket,
    },
};
use gmdns::{MdnsPacket, parser::record::endpoint::EndpointAddr as DnsEndpointAddr};
use snafu::Report;
use tokio::time::{self, MissedTickBehavior, interval};
use tokio_util::task::AbortOnDropHandle;
use tracing::{info, warn};

use crate::publisher::ServerConfig;

const STUN_RECONCILE_INTERVAL: Duration = Duration::from_secs(30);
const STUN_PUBLISH_INTERVAL: Duration = Duration::from_secs(10);
pub const STUN_DOMAIN: &str = "stun.genmeta.net";

// ──────────────────────────────────────────────────────────────────────────────
// 数据结构
// ──────────────────────────────────────────────────────────────────────────────

/// 本节点 STUN 配置（从配置文件 `location /stun { ... }` 解析）
///
/// 角色由配置内容决定：
/// - 配置了 `bind` 块（含 `outer`）→ Bootstrap 节点（公网，直接启动 STUN server）
/// - 无 `bind` 块 → Dynamic 节点（NAT 后，等探测到 FullCone 后启动）
///
/// `relay`：
/// - `true`（`relay on;`）→ 加入 forward 中转组件
/// - `false`（`relay off;`）→ 仅支持 STUN 探测，不参与中转
#[derive(Debug, Clone)]
pub struct StunNodeConfig {
    /// 是否加入 forward 中转网络（默认 false）
    pub relay: bool,
    /// 每个 bind 块的配置（支持多地址族 / 多绑定）
    pub binds: Vec<StunBindConfig>,
}

/// 单个 bind 块的 STUN 配置
#[derive(Debug, Clone)]
pub struct StunBindConfig {
    /// 绑定地址
    pub bind_address: SocketAddr,
    /// 对外发布地址
    pub outer_address: Option<SocketAddr>,
    /// change_address
    pub change_address: Option<SocketAddr>,
    /// 辅助端口
    pub change_port: Option<u16>,
}

impl StunNodeConfig {
    /// 是否配置了 bind 块
    pub fn has_configured_binds(&self) -> bool {
        !self.binds.is_empty()
    }
}

/// 管理 STUN server：
///
/// 1. **Configured pairs**（来自 `bind` 块）：独立绑定 socket，与 QUIC listener 无关，始终启动
/// 2. **Dynamic pairs**（来自 QUIC listener）：探测到 FullCone 后在 QUIC 端口上启动 STUN server
///
/// 两类地址发布到 stun.genmeta.net
pub struct StunServerManager {
    _task: AbortOnDropHandle<()>,
}

/// Configured bind 块创建的独立 STUN server 对（拥有自己的 socket）
struct ConfiguredStunHandle {
    _main_iface: Arc<dyn IO>,
    _main_recv_task: AbortOnDropHandle<std::io::Result<()>>,
    _aux_iface: Arc<dyn IO>,
    _aux_recv_task: AbortOnDropHandle<std::io::Result<()>>,
    _server_main: AbortOnDropHandle<std::io::Result<()>>,
    _server_aux: AbortOnDropHandle<std::io::Result<()>>,
}

impl ConfiguredStunHandle {
    fn is_alive(&self) -> bool {
        !self._server_main.is_finished()
            && !self._server_aux.is_finished()
            && !self._main_recv_task.is_finished()
            && !self._aux_recv_task.is_finished()
    }
}

/// QUIC listener 上寄生的 STUN server 对（FullCone 确认后才启动）
struct DynamicStunHandle {
    _aux_iface: Arc<dyn IO>,
    _aux_recv_task: AbortOnDropHandle<std::io::Result<()>>,
    _server_main: AbortOnDropHandle<std::io::Result<()>>,
    _server_aux: AbortOnDropHandle<std::io::Result<()>>,
}

impl DynamicStunHandle {
    fn is_alive(&self) -> bool {
        !self._server_main.is_finished() && !self._server_aux.is_finished()
    }
}

impl StunServerManager {
    pub fn spawn(
        listeners: Arc<QuicListeners>,
        publish_config: ServerConfig,
        config: StunNodeConfig,
    ) -> Self {
        let _task = AbortOnDropHandle::new(tokio::spawn(async move {
            let mut configured_handles: HashMap<SocketAddr, ConfiguredStunHandle> = HashMap::new();
            let mut dynamic_handles: HashMap<BindUri, DynamicStunHandle> = HashMap::new();
            let use_configured = config.has_configured_binds();

            macro_rules! reconcile {
                () => {
                    if use_configured {
                        reconcile_configured(&config, &mut configured_handles);
                    } else {
                        reconcile_dynamic(&listeners, &mut dynamic_handles).await;
                    }
                };
            }

            reconcile!();
            publish_stun_endpoints(
                &listeners,
                &configured_handles,
                &dynamic_handles,
                &publish_config,
                &config,
            )
            .await;

            let mut timer = interval(STUN_RECONCILE_INTERVAL);
            timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
            let mut publish_timer = interval(STUN_PUBLISH_INTERVAL);
            publish_timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
            let mut observer = Locations::global().subscribe();

            loop {
                tokio::select! {
                    _ = observer.recv() => {
                        time::sleep(Duration::from_millis(50)).await;
                        reconcile!();
                        publish_stun_endpoints(&listeners, &configured_handles, &dynamic_handles, &publish_config, &config).await;
                    }
                    _ = timer.tick() => {
                        reconcile!();
                        publish_stun_endpoints(&listeners, &configured_handles, &dynamic_handles, &publish_config, &config).await;
                    }
                    _ = publish_timer.tick() => {
                        publish_stun_endpoints(&listeners, &configured_handles, &dynamic_handles, &publish_config, &config).await;
                    }
                }
            }
        }));
        Self { _task }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Reconcile: Configured（bind 块 → 独立 socket）
// ──────────────────────────────────────────────────────────────────────────────

/// 为每个 `bind` 块创建/恢复独立的 STUN server 对
fn reconcile_configured(
    config: &StunNodeConfig,
    handles: &mut HashMap<SocketAddr, ConfiguredStunHandle>,
) {
    // 移除不在配置中或已失效的
    let configured_addrs: HashSet<SocketAddr> =
        config.binds.iter().map(|b| b.bind_address).collect();
    handles.retain(|addr, h| configured_addrs.contains(addr) && h.is_alive());

    for bind_cfg in &config.binds {
        if handles.contains_key(&bind_cfg.bind_address) {
            continue;
        }
        match start_configured_stun_pair(bind_cfg) {
            Some(handle) => {
                info!(target: "stun", bind = %bind_cfg.bind_address, "Started configured STUN server pair");
                handles.insert(bind_cfg.bind_address, handle);
            }
            None => {
                warn!(target: "stun", bind = %bind_cfg.bind_address, "Failed to start configured STUN server pair, will retry");
            }
        }
    }
}

/// 在配置的地址上绑定独立 socket 并启动 STUN server 对
fn start_configured_stun_pair(bind_cfg: &StunBindConfig) -> Option<ConfiguredStunHandle> {
    let factory: Arc<dyn ProductIO> = Arc::new(DEFAULT_IO_FACTORY);

    // 主 socket
    let main_iface: Arc<dyn IO> = Arc::from(factory.bind(bind_cfg.bind_address.into()));
    let main_port = match main_iface.bound_addr() {
        Ok(BoundAddr::Internet(addr)) => addr.port(),
        _ => {
            warn!(target: "stun", bind = %bind_cfg.bind_address, "Configured main iface has no internet bound addr");
            return None;
        }
    };
    let main_router = StunRouter::new();
    let _main_recv_task = ReceiveAndDeliverPacket::task()
        .stun_router(main_router.clone())
        .iface_ref(main_iface.clone())
        .spawn();

    // 辅 socket
    let aux_port_cfg = bind_cfg.change_port.unwrap_or(0);
    let aux_addr = SocketAddr::new(bind_cfg.bind_address.ip(), aux_port_cfg);
    let aux_iface: Arc<dyn IO> = Arc::from(factory.bind(aux_addr.into()));
    let aux_port = match aux_iface.bound_addr() {
        Ok(BoundAddr::Internet(addr)) => addr.port(),
        _ => {
            warn!(target: "stun", "Configured aux iface has no internet bound addr");
            return None;
        }
    };
    let aux_router = StunRouter::new();
    let _aux_recv_task = ReceiveAndDeliverPacket::task()
        .stun_router(aux_router.clone())
        .iface_ref(aux_iface.clone())
        .spawn();

    // 主 STUN server：change_port 指向辅助端口
    let server_main = StunServer::new(
        main_iface.clone(),
        main_router,
        StunServerConfig::builder()
            .change_port(aux_port)
            .maybe_change_address(bind_cfg.change_address)
            .init(),
    );
    // 辅 STUN server：change_port 指向主端口
    let server_aux = StunServer::new(
        aux_iface.clone(),
        aux_router,
        StunServerConfig::builder()
            .change_port(main_port)
            .maybe_change_address(bind_cfg.change_address)
            .init(),
    );

    Some(ConfiguredStunHandle {
        _main_iface: main_iface,
        _main_recv_task,
        _aux_iface: aux_iface,
        _aux_recv_task,
        _server_main: server_main.spawn(),
        _server_aux: server_aux.spawn(),
    })
}

// ──────────────────────────────────────────────────────────────────────────────
// Reconcile: Dynamic（QUIC listener FullCone → 寄生 STUN server）
// ──────────────────────────────────────────────────────────────────────────────

/// 在 FullCone 确认的 QUIC listener 上启动/恢复 STUN server 对
async fn reconcile_dynamic(
    listeners: &Arc<QuicListeners>,
    handles: &mut HashMap<BindUri, DynamicStunHandle>,
) {
    let desired = collect_fullcone_bind_uris(listeners);

    // 移除不再 FullCone 或已失效的
    handles.retain(|uri, h| desired.contains(uri) && h.is_alive());

    for bind_uri in &desired {
        if handles.contains_key(bind_uri) {
            continue;
        }

        let Some(bind_iface) = find_bind_iface(listeners, bind_uri) else {
            continue;
        };

        match start_dynamic_stun_pair(bind_uri, &bind_iface) {
            Some(handle) => {
                info!(target: "stun", %bind_uri, "Started dynamic STUN server pair (FullCone)");
                handles.insert(bind_uri.clone(), handle);
            }
            None => {
                warn!(target: "stun", %bind_uri, "Failed to start dynamic STUN server pair, will retry");
            }
        }
    }
}

fn find_bind_iface(listeners: &Arc<QuicListeners>, bind_uri: &BindUri) -> Option<BindInterface> {
    for server_name in listeners.servers() {
        let Some(server) = listeners.get_server(&server_name) else {
            continue;
        };
        if let Some(iface) = server.get_iface(bind_uri) {
            return Some(iface);
        }
    }
    None
}

/// 收集 FullCone 确认的 QUIC listener BindUri
fn collect_fullcone_bind_uris(listeners: &Arc<QuicListeners>) -> HashSet<BindUri> {
    let mut result = HashSet::new();
    for server_name in listeners.servers() {
        let Some(server) = listeners.get_server(&server_name) else {
            continue;
        };
        for (bind_uri, bind_iface) in server.bind_interfaces() {
            let is_fullcone = bind_iface
                .borrow()
                .with_component(|clients: &StunClientsComponent| {
                    clients.with_clients(|map| {
                        map.values()
                            .any(|c| matches!(c.get_nat_type(), Some(Ok(NatType::FullCone))))
                    })
                })
                .ok()
                .flatten()
                .unwrap_or(false);
            if is_fullcone {
                result.insert(bind_uri);
            }
        }
    }
    result
}

/// 在 QUIC listener 接口上寄生 STUN server 对（共享 QUIC socket）
fn start_dynamic_stun_pair(
    bind_uri: &BindUri,
    bind_iface: &BindInterface,
) -> Option<DynamicStunHandle> {
    let iface = bind_iface.borrow();

    let main_addr = match iface.bound_addr() {
        Ok(BoundAddr::Internet(addr)) => addr,
        _ => {
            warn!(target: "stun", %bind_uri, "Main iface has no internet bound addr");
            return None;
        }
    };

    // 从 StunRouterComponent 取出主 StunRouter
    let stun_router_main = iface
        .with_component(|comp: &StunRouterComponent| comp.router())
        .ok()
        .flatten()?;

    // WeakInterface 作为主 StunServer 的 RefIO（共享 QUIC socket）
    let weak_iface: WeakInterface = bind_iface.borrow_weak();
    let main_port = main_addr.port();
    let main_is_ipv4 = is_ipv4_bind_uri(bind_uri);

    // 动态选取 change_address
    let change_address = compute_change_address(&iface, main_is_ipv4);

    // 辅助端口：port=0 随机分配
    let aux_bind_uri = derive_aux_bind_uri(bind_uri, None)?;

    let factory: Arc<dyn ProductIO> = Arc::new(DEFAULT_IO_FACTORY);
    let aux_iface: Arc<dyn IO> = Arc::from(factory.bind(aux_bind_uri));
    let aux_port = match aux_iface.bound_addr() {
        Ok(BoundAddr::Internet(addr)) => addr.port(),
        _ => {
            warn!(target: "stun", "Aux iface has no internet bound addr");
            return None;
        }
    };

    let stun_router_aux = StunRouter::new();
    let _aux_recv_task = ReceiveAndDeliverPacket::task()
        .stun_router(stun_router_aux.clone())
        .iface_ref(aux_iface.clone())
        .spawn();

    let server_main = StunServer::new(
        weak_iface,
        stun_router_main,
        StunServerConfig::builder()
            .change_port(aux_port)
            .maybe_change_address(change_address)
            .init(),
    );

    let server_aux = StunServer::new(
        aux_iface.clone(),
        stun_router_aux,
        StunServerConfig::builder()
            .change_port(main_port)
            .maybe_change_address(change_address)
            .init(),
    );

    Some(DynamicStunHandle {
        _aux_iface: aux_iface,
        _aux_recv_task,
        _server_main: server_main.spawn(),
        _server_aux: server_aux.spawn(),
    })
}

/// 判断 BindUri 是否是 IPv4
fn is_ipv4_bind_uri(bind_uri: &BindUri) -> bool {
    if let Some((family, _, _)) = bind_uri.as_iface_bind_uri() {
        family == Family::V4
    } else if let Some(addr) = bind_uri.as_inet_bind_uri() {
        addr.is_ipv4()
    } else {
        true
    }
}

/// 派生辅助 BindUri：同 scheme/family/device
/// 使用配置端口（fixed_port），None 时回退到 port=0 随机分配
fn derive_aux_bind_uri(bind_uri: &BindUri, fixed_port: Option<u16>) -> Option<BindUri> {
    let port = fixed_port.unwrap_or(0);
    match bind_uri.scheme() {
        BindUriScheme::Iface => {
            let (family, device, _port) = bind_uri.as_iface_bind_uri()?;
            Some(format!("iface://{family}.{device}:{port}").into())
        }
        BindUriScheme::Inet => {
            let addr = bind_uri.as_inet_bind_uri()?;
            Some(SocketAddr::new(addr.ip(), port).into())
        }
        _ => None,
    }
}

/// 从当前 iface 的 StunClientsComponent 中选取 change_address
///
/// 使用 `by_port_ip_asc` 策略形成单向环，避免互指：
/// 1. 从活跃 stun client 的 agent 中筛选同地址族
/// 2. 排除自身 outer IP
/// 3. 按 (port, ip) 排序，取"大于本机"的最小候选作为后继
/// 4. 若无更大候选，回绕到排序最小的节点
fn compute_change_address(
    iface: &gm_quic::qinterface::Interface,
    is_ipv4: bool,
) -> Option<SocketAddr> {
    let own_outer_ips: HashSet<IpAddr> = iface
        .with_component(|clients: &StunClientsComponent| {
            clients.with_clients(|map| {
                map.values()
                    .filter_map(|c| c.get_outer_addr()?.ok())
                    .map(|a| a.ip())
                    .collect::<HashSet<_>>()
            })
        })
        .ok()
        .flatten()
        .unwrap_or_default();

    // 获取本机的 outer addr（用于 by_port_ip_asc 排序比较）
    let own_outer_addr: Option<SocketAddr> = iface
        .with_component(|clients: &StunClientsComponent| {
            clients.with_clients(|map| {
                map.values()
                    .filter_map(|c| c.get_outer_addr()?.ok())
                    .find(|a| a.is_ipv4() == is_ipv4)
            })
        })
        .ok()
        .flatten()
        .flatten();

    // 收集所有候选 agent 地址（同地址族，不在本机 outer IP 集合中）
    let mut candidates: Vec<SocketAddr> = iface
        .with_component(|clients: &StunClientsComponent| {
            clients.with_clients(|map| {
                map.keys()
                    .copied()
                    .filter(|agent| agent.is_ipv4() == is_ipv4)
                    .filter(|agent| !own_outer_ips.contains(&agent.ip()))
                    .collect::<Vec<_>>()
            })
        })
        .ok()
        .flatten()
        .unwrap_or_default();

    if candidates.is_empty() {
        return None;
    }

    // 按 (port, ip) 排序
    candidates.sort_by_key(|a| (a.port(), a.ip()));

    // by_port_ip_asc：取"大于本机"的最小候选，无更大候选则回绕到最小
    if let Some(own) = own_outer_addr {
        candidates
            .iter()
            .find(|c| (c.port(), c.ip()) > (own.port(), own.ip()))
            .or_else(|| candidates.first())
            .copied()
    } else {
        // 没有本机 outer addr 时，取排序最小的候选
        candidates.first().copied()
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Publish
// ──────────────────────────────────────────────────────────────────────────────

async fn publish_stun_endpoints(
    listeners: &Arc<QuicListeners>,
    configured_handles: &HashMap<SocketAddr, ConfiguredStunHandle>,
    dynamic_handles: &HashMap<BindUri, DynamicStunHandle>,
    publish_config: &ServerConfig,
    config: &StunNodeConfig,
) {
    if publish_config.resolvers.is_empty() {
        warn!(target: "stun", "STUN endpoints found but no DNS publisher resolver available, cannot publish");
        return;
    }

    let mut outer_addrs: HashSet<SocketAddr> = HashSet::new();

    // 1. Configured bind 块：直接用配置的 outer_address（或 bind_address）
    for bind_cfg in &config.binds {
        if !configured_handles.contains_key(&bind_cfg.bind_address) {
            continue; // 未成功启动的不上报
        }
        if let Some(addr) = bind_cfg.outer_address.or(Some(bind_cfg.bind_address)) {
            outer_addrs.insert(addr);
        }
    }

    // 2. Dynamic（QUIC listener FullCone）：仅上报 FullCone 确认的外网地址
    for bind_uri in dynamic_handles.keys() {
        let Some(bind_iface) = find_bind_iface(listeners, bind_uri) else {
            continue;
        };
        let iface = bind_iface.borrow();
        let fullcone_outers: Vec<SocketAddr> = iface
            .with_component(|clients: &StunClientsComponent| {
                clients.with_clients(|map| {
                    map.values()
                        .filter(|c| matches!(c.get_nat_type(), Some(Ok(NatType::FullCone))))
                        .filter_map(|c| c.get_outer_addr()?.ok())
                        .collect::<Vec<_>>()
                })
            })
            .ok()
            .flatten()
            .unwrap_or_default();
        outer_addrs.extend(fullcone_outers);
    }

    if outer_addrs.is_empty() {
        warn!(target: "stun", "No STUN endpoints to publish");
        return;
    }

    let mut endpoints: Vec<DnsEndpointAddr> = outer_addrs
        .iter()
        .map(|addr| match addr {
            SocketAddr::V4(a) => DnsEndpointAddr::direct_v4(*a),
            SocketAddr::V6(a) => DnsEndpointAddr::direct_v6(*a),
        })
        .collect();
    endpoints
        .iter_mut()
        .for_each(|ep| publish_config.sign_endpoint(ep));

    let mut hosts = HashMap::new();
    hosts.insert(STUN_DOMAIN.to_string(), endpoints);
    let packet = MdnsPacket::answer(0, &hosts).to_bytes();

    for publisher in &publish_config.resolvers {
        if let Err(e) = publisher.publish(STUN_DOMAIN, &packet).await {
            warn!(
                target: "stun",
                "Failed to publish stun endpoints to {}: {}",
                STUN_DOMAIN,
                Report::from_error(e)
            );
        } else {
            info!(
                target: "stun",
                count = outer_addrs.len(),
                "Published STUN endpoints to {STUN_DOMAIN}"
            );
        }
    }
}
