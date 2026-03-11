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
use gmdns::parser::record::endpoint::EndpointAddr as DnsEndpointAddr;
use tokio::time::{self, MissedTickBehavior, interval};
use tokio_util::task::AbortOnDropHandle;
use tracing::{info, warn};

use crate::{
    dns::{PublishConfig, publish_host_endpoints},
    parse::{Node, Value},
};

const STUN_RECONCILE_INTERVAL: Duration = Duration::from_secs(30);
const STUN_PUBLISH_INTERVAL: Duration = Duration::from_secs(10);
pub const STUN_DOMAIN: &str = "stun.genmeta.net";

// ──────────────────────────────────────────────────────────────────────────────
// 数据结构
// ──────────────────────────────────────────────────────────────────────────────

/// 本节点的 STUN 运行时配置。
///
/// 这不是配置文件 AST 的原样映射，而是把 `server` 块里的以下配置归一化后的结果：
/// - `stun on|off;`
/// - `relay on|off;`
/// - 多个 `stun_server { ... }`
///
/// 最终行为由内容决定：
/// - 配置了 `stun_server { ... }` → 走 configured 模式，独立绑定主/辅 socket
/// - 只有 `stun on;` → 走 dynamic 模式，等本地 listener 被判定为 `FullCone` 后寄生启动
///
/// `relay` 目前只是配置侧保留字段，本文件中的 STUN server 生命周期并不会直接使用它。
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
    /// STUN `ChangedAddress` / `CHANGE_IP` 对应的完整替代地址（包含 IP 和端口）
    pub change_address: Option<SocketAddr>,
    /// 辅助端口
    pub change_port: Option<u16>,
}

impl StunNodeConfig {
    /// 从 `server` 配置节点提取 STUN 相关配置。
    ///
    /// 两种启用方式满足其一即可：
    /// - `stun on;`
    /// - 存在至少一个 `stun_server { ... }`
    pub fn from_server_node(server: &Arc<Node>) -> Option<Self> {
        let stun_enabled = server
            .get("stun")
            .and_then(|v| match v {
                Value::Boolean(b) => Some(*b),
                _ => None,
            })
            .unwrap_or(false);

        let binds: Vec<StunBindConfig> = match server.get("stun_server") {
            Some(Value::Nodes(nodes)) => nodes
                .iter()
                .filter_map(|node| {
                    let Value::ValueMap(map) = node.value() else {
                        return None;
                    };
                    let bind_address = match map.get("bind")? {
                        Value::Addr(addr) => *addr,
                        _ => return None,
                    };
                    let outer_address = map.get("outer_addr").and_then(|v| match v {
                        Value::Addr(addr) => Some(*addr),
                        _ => None,
                    });
                    let change_address = map.get("change_addr").and_then(|v| match v {
                        Value::Addr(addr) => Some(*addr),
                        _ => None,
                    });
                    let change_port = map.get("change_port").and_then(|v| match v {
                        Value::String(s) => s.parse::<u16>().ok(),
                        _ => None,
                    });
                    Some(StunBindConfig {
                        bind_address,
                        outer_address,
                        change_address,
                        change_port,
                    })
                })
                .collect(),
            _ => vec![],
        };

        if !stun_enabled && binds.is_empty() {
            return None;
        }

        let relay = server
            .get("relay")
            .and_then(|v| match v {
                Value::Boolean(b) => Some(*b),
                _ => None,
            })
            .unwrap_or(false);

        Some(Self { relay, binds })
    }

    /// 是否配置了 bind 块
    pub fn has_configured_binds(&self) -> bool {
        !self.binds.is_empty()
    }
}

/// 管理本节点对外暴露的 STUN server。
///
/// 1. **Configured pairs**：来自 `stun_server { ... }`，独立绑定主/辅 socket
/// 2. **Dynamic pairs**：来自 QUIC listener，仅在该 listener 已被判定为 `FullCone` 时寄生启动
///
/// 管理器同时负责收集“可作为完整 STUN agent 使用”的地址并交给 DNS 模块发布。
pub struct StunServerManager {
    _task: AbortOnDropHandle<()>,
}

/// configured 模式下的一对 STUN server，主/辅 socket 都由本模块独立持有。
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

/// dynamic 模式下的一对 STUN server：主 server 复用 QUIC socket，辅 server 独立绑定辅助端口。
struct DynamicStunHandle {
    /// 当前这对 server 对外宣告的 `ChangedAddress`
    change_address: Option<SocketAddr>,
    _aux_iface: Arc<dyn IO>,
    _aux_recv_task: AbortOnDropHandle<std::io::Result<()>>,
    _server_main: AbortOnDropHandle<std::io::Result<()>>,
    _server_aux: AbortOnDropHandle<std::io::Result<()>>,
}

impl DynamicStunHandle {
    fn is_alive(&self) -> bool {
        !self._server_main.is_finished()
            && !self._server_aux.is_finished()
            && !self._aux_recv_task.is_finished()
    }
}

impl StunServerManager {
    pub fn spawn(
        listeners: Arc<QuicListeners>,
        publish_config: PublishConfig,
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
// Reconcile: Configured（`stun_server` → 独立 socket）
// ──────────────────────────────────────────────────────────────────────────────

/// 为每个 `stun_server` 配置创建或恢复一对独立的 STUN server。
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

/// 在配置地址上绑定主/辅 socket，并让两侧通过 `change_port` 互相指向。
///
/// 若 `change_address` 为空，则该 pair 只能支持同 IP 下的 `CHANGE_PORT`，
/// 不能为客户端提供完整的 `ChangedAddress` / `CHANGE_IP` 能力。
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

    // 主 server 对外宣告“改端口”时落到辅助端口；是否支持改 IP 取决于 `change_address`
    let server_main = StunServer::new(
        main_iface.clone(),
        main_router,
        StunServerConfig::builder()
            .change_port(aux_port)
            .maybe_change_address(bind_cfg.change_address)
            .init(),
    );
    // 辅 server 则反向指回主端口
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
// Reconcile: Dynamic（`FullCone` QUIC listener → 寄生 STUN server）
// ──────────────────────────────────────────────────────────────────────────────

/// 为已判定为 `FullCone` 的 QUIC listener 启动或刷新动态 STUN pair。
///
/// 除了存活性，这里还会检查 `change_address` 是否变化；
/// 一旦环上后继变化，就重建 pair，让新配置立即生效。
async fn reconcile_dynamic(
    listeners: &Arc<QuicListeners>,
    handles: &mut HashMap<BindUri, DynamicStunHandle>,
) {
    let desired = collect_fullcone_bind_uris(listeners);

    // 移除不再 FullCone 或已失效的
    handles.retain(|uri, h| desired.contains(uri) && h.is_alive());

    for bind_uri in &desired {
        if handles.contains_key(bind_uri) {
            let Some(bind_iface) = find_bind_iface(listeners, bind_uri) else {
                continue;
            };
            let is_ipv4 = is_ipv4_bind_uri(bind_uri);
            let expected_change_address = {
                let iface = bind_iface.borrow();
                compute_change_address(&iface, is_ipv4)
            };
            let needs_restart = handles
                .get(bind_uri)
                .is_some_and(|handle| handle.change_address != expected_change_address);
            if !needs_restart {
                continue;
            }
            handles.remove(bind_uri);

            match start_dynamic_stun_pair(bind_uri, &bind_iface, expected_change_address) {
                Some(handle) => {
                    info!(target: "stun", %bind_uri, ?expected_change_address, "Restarted dynamic STUN server pair after change_address update");
                    handles.insert(bind_uri.clone(), handle);
                }
                None => {
                    warn!(target: "stun", %bind_uri, "Failed to restart dynamic STUN server pair, will retry");
                }
            }
            continue;
        }

        let Some(bind_iface) = find_bind_iface(listeners, bind_uri) else {
            continue;
        };
        let is_ipv4 = is_ipv4_bind_uri(bind_uri);
        let change_address = {
            let iface = bind_iface.borrow();
            compute_change_address(&iface, is_ipv4)
        };

        match start_dynamic_stun_pair(bind_uri, &bind_iface, change_address) {
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

/// 收集所有当前已被任一 STUN client 判定为 `FullCone` 的 listener。
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

/// 在现有 QUIC listener 上寄生一对 STUN server。
///
/// - 主 server 复用 listener 自身的 socket
/// - 辅 server 额外绑定一个同设备/同地址族的辅助端口
/// - `change_address` 由 `compute_change_address()` 动态选出，代表“切到另一台 agent”时的目标地址
fn start_dynamic_stun_pair(
    bind_uri: &BindUri,
    bind_iface: &BindInterface,
    change_address: Option<SocketAddr>,
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

    // WeakInterface 作为主 server 的 RefIO，直接复用 QUIC listener 的 socket
    let weak_iface: WeakInterface = bind_iface.borrow_weak();
    let main_port = main_addr.port();

    // 辅助端口使用同设备/同地址族的新 bind，端口交给系统分配
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
        change_address,
        _aux_iface: aux_iface,
        _aux_recv_task,
        _server_main: server_main.spawn(),
        _server_aux: server_aux.spawn(),
    })
}

/// 判断一个 `BindUri` 最终绑定的是 IPv4 还是 IPv6。
fn is_ipv4_bind_uri(bind_uri: &BindUri) -> bool {
    if let Some((family, _, _)) = bind_uri.as_iface_bind_uri() {
        family == Family::V4
    } else if let Some(addr) = bind_uri.as_inet_bind_uri() {
        addr.is_ipv4()
    } else {
        true
    }
}

/// 基于主 listener 派生辅助 bind：保持 scheme / family / device 不变，仅替换端口。
///
/// `fixed_port = None` 时使用 `0`，交给系统随机分配端口。
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

/// 从当前 iface 已解析成功的 STUN client 中挑选下一个 `change_address`。
///
/// 这里选的是“其他 STUN agent 的公开入口地址”，不是本机的辅助端口：
/// 1. 只使用已成功拿到 outer address 的 client，对应当前可达的 agent
/// 2. 排除 IP 落在“本机已观测 outer IP 集合”中的 agent，避免把请求转回自己
/// 3. 以 `(port, ip)` 为序，为当前节点选一个稳定的后继，形成单向环
/// 4. 若没有严格更大的候选，则回绕到最小候选
///
/// 注意：这只是“选下一跳”的策略，不保证 `CHANGE_IP|CHANGE_PORT` 一定同时改变 IP 和端口；
/// 真正返回哪个地址取决于被选中节点自身的 `change_address` / `change_port` 配置。
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

    // 取一个当前已解析成功的 outer addr 作为环上的“本机坐标”
    let own_outer_addr: Option<SocketAddr> = iface
        .with_component(|clients: &StunClientsComponent| {
            clients.with_clients(|map| {
                map.values()
                    .filter_map(|c| c.get_outer_addr()?.ok())
                    .filter(|a| a.is_ipv4() == is_ipv4)
                    .min_by_key(|a| (a.port(), a.ip()))
            })
        })
        .ok()
        .flatten()
        .flatten();

    // 收集所有“当前可达”的候选 agent 地址（同地址族，不在本机 outer IP 集合中）
    let mut candidates: Vec<SocketAddr> = iface
        .with_component(|clients: &StunClientsComponent| {
            clients.with_clients(|map| {
                map.iter()
                    .filter_map(|(agent, client)| client.get_outer_addr()?.ok().map(|_| *agent))
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
// Publish（仅负责收集 STUN agent 地址；真正的 DNS 发布委托给 dns 模块）
// ──────────────────────────────────────────────────────────────────────────────

async fn publish_stun_endpoints(
    listeners: &Arc<QuicListeners>,
    configured_handles: &HashMap<SocketAddr, ConfiguredStunHandle>,
    dynamic_handles: &HashMap<BindUri, DynamicStunHandle>,
    publish_config: &PublishConfig,
    config: &StunNodeConfig,
) {
    let mut outer_addrs: HashSet<SocketAddr> = HashSet::new();

    // 1. configured 模式：发布显式配置的对外地址
    for bind_cfg in &config.binds {
        if !configured_handles.contains_key(&bind_cfg.bind_address) {
            continue; // 未成功启动的不上报
        }
        if bind_cfg.change_address.is_none() {
            continue; // 缺少 ChangedAddress，不能作为完整 STUN agent 对外发布
        }
        if let Some(addr) = bind_cfg.outer_address.or(Some(bind_cfg.bind_address)) {
            outer_addrs.insert(addr);
        }
    }

    // 2. dynamic 模式：发布 FullCone listener 当前探测到的外网地址
    for (bind_uri, handle) in dynamic_handles {
        if handle.change_address.is_none() {
            continue; // 缺少 ChangedAddress 时仅具备部分 STUN 能力，不发布到公共 agent 列表
        }
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

    let endpoints: Vec<DnsEndpointAddr> = outer_addrs
        .iter()
        .map(|addr| match addr {
            SocketAddr::V4(a) => DnsEndpointAddr::direct_v4(*a),
            SocketAddr::V6(a) => DnsEndpointAddr::direct_v6(*a),
        })
        .collect();

    publish_host_endpoints(STUN_DOMAIN, endpoints, publish_config).await;
}
