use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use gm_quic::{
    prelude::{BindUri, BoundAddr, IO, QuicListeners},
    qbase::net::Family,
    qdns::Publish as DnsPublisher,
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

const STUN_RECONCILE_INTERVAL: Duration = Duration::from_secs(30);
const STUN_PUBLISH_INTERVAL: Duration = Duration::from_secs(10);
const STUN_DOMAIN: &str = "stun.genmeta.net";

// ──────────────────────────────────────────────────────────────────────────────
// 数据结构
// ──────────────────────────────────────────────────────────────────────────────

/// 本节点 STUN 配置（从配置文件 `location /stun { ... }` 解析）
///
/// 角色由配置内容决定：
/// - 配置了 `outer` 等参数 → Bootstrap 节点（公网，直接启动 STUN server）
/// - 未配置 `outer` → Dynamic 节点（NAT 后，等探测到 FullCone 后启动）
///
/// `relay`：
/// - `true`（`relay on;`）→ 加入 forward 中转组件
/// - `false`（`relay off;`）→ 仅支持 STUN 探测，不参与中转
#[derive(Debug, Clone)]
pub struct StunNodeConfig {
    /// 主绑定地址（可选，默认取 listen 地址）
    pub bind_address: Option<SocketAddr>,
    /// 辅助端口（可选，None 时绑定 port=0 随机分配）
    pub change_port: Option<u16>,
    /// 对外发布地址（用于 forward 中转组件上报可达地址）
    pub outer_address: Option<SocketAddr>,
    /// change_address（可选，bootstrap 建议配置）
    pub change_address: Option<SocketAddr>,
    /// 是否加入 forward 中转网络（默认 false）
    pub relay: bool,
}

impl StunNodeConfig {
    /// 是否为 Bootstrap 节点（配置了 outer 地址）
    pub fn is_bootstrap(&self) -> bool {
        self.outer_address.is_some()
    }
}

/// 管理每个 iface 上一对 StunServer（主端口 + 辅助端口）
/// bootstrap 节点立即启动 server，dynamic 节点等待 FullCone 确认后启动
/// 将外网地址发布到 stun.genmeta.net
pub struct StunServerManager {
    _task: AbortOnDropHandle<()>,
}

struct IfaceStunHandle {
    _aux_iface: Arc<dyn IO>,
    _aux_recv_task: AbortOnDropHandle<std::io::Result<()>>,
    _server_main: AbortOnDropHandle<std::io::Result<()>>,
    _server_aux: AbortOnDropHandle<std::io::Result<()>>,
}

impl IfaceStunHandle {
    fn is_alive(&self) -> bool {
        !self._server_main.is_finished() && !self._server_aux.is_finished()
    }
}

impl StunServerManager {
    /// 启动 StunServerManager，持久持有返回值
    pub fn spawn(
        listeners: Arc<QuicListeners>,
        publishers: Vec<Arc<dyn DnsPublisher + Send + Sync>>,
        config: StunNodeConfig,
    ) -> Self {
        let _task = AbortOnDropHandle::new(tokio::spawn(async move {
            let mut handles: HashMap<BindUri, IfaceStunHandle> = HashMap::new();

            // Bootstrap 首次拉起
            reconcile_stun_servers(&listeners, &mut handles, &config).await;
            publish_stun_endpoints(&listeners, &handles, &publishers, &config).await;

            let mut timer = interval(STUN_RECONCILE_INTERVAL);
            timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
            let mut publish_timer = interval(STUN_PUBLISH_INTERVAL);
            publish_timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
            let mut observer = Locations::global().subscribe();

            loop {
                tokio::select! {
                    _ = observer.recv() => {
                        // 防抖 50ms
                        time::sleep(Duration::from_millis(50)).await;
                        reconcile_stun_servers(&listeners, &mut handles, &config).await;
                        publish_stun_endpoints(&listeners, &handles, &publishers, &config).await;
                    }
                    _ = timer.tick() => {
                        reconcile_stun_servers(&listeners, &mut handles, &config).await;
                        publish_stun_endpoints(&listeners, &handles, &publishers, &config).await;
                    }
                    _ = publish_timer.tick() => {
                        publish_stun_endpoints(&listeners, &handles, &publishers, &config).await;
                    }
                }
            }
        }));
        Self { _task }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Reconcile
// ──────────────────────────────────────────────────────────────────────────────

async fn reconcile_stun_servers(
    listeners: &Arc<QuicListeners>,
    handles: &mut HashMap<BindUri, IfaceStunHandle>,
    config: &StunNodeConfig,
) {
    let desired = collect_stun_eligible_bind_uris(listeners, config);

    // 移除不再需要的
    handles.retain(|uri, _| desired.contains(uri));

    // 为 desired 集合中没有存活 handle 的 bind_uri 拉起新对
    for bind_uri in &desired {
        let needs_start = handles.get(bind_uri).map(|h| !h.is_alive()).unwrap_or(true);

        if !needs_start {
            continue;
        }

        // 移除已失效 handle（若存在）
        handles.remove(bind_uri);

        let Some(bind_iface) = find_bind_iface(listeners, bind_uri) else {
            continue;
        };

        match start_stun_pair(bind_uri, &bind_iface, config) {
            Some(handle) => {
                let role = if config.is_bootstrap() {
                    "bootstrap"
                } else {
                    "dynamic"
                };
                info!(target: "stun", %bind_uri, role, "Started STUN server pair");
                handles.insert(bind_uri.clone(), handle);
            }
            None => {
                warn!(target: "stun", %bind_uri, "Failed to start STUN server pair, will retry");
            }
        }
    }
}

/// 收集当前所有 eligible iface 的 BindUri
///
/// - bootstrap 节点：只要 iface 绑定了公网地址即可，不需要等待 client 探测
/// - dynamic 节点：需要 client 探测出 FullCone 才启动
fn collect_stun_eligible_bind_uris(
    listeners: &Arc<QuicListeners>,
    config: &StunNodeConfig,
) -> HashSet<BindUri> {
    let mut result = HashSet::new();
    for server_name in listeners.servers() {
        let Some(server) = listeners.get_server(&server_name) else {
            continue;
        };
        for (bind_uri, bind_iface) in server.bind_interfaces() {
            let bound_ok = bind_iface
                .borrow()
                .bound_addr()
                .map(|addr| matches!(addr, BoundAddr::Internet(_)))
                .unwrap_or(false);
            if !bound_ok {
                continue;
            }

            if config.is_bootstrap() {
                // bootstrap 节点：绑定了公网地址即可，直接 eligible
                result.insert(bind_uri);
            } else {
                // dynamic 节点：需要 client 探测出 FullCone
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
    }
    result
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

// ──────────────────────────────────────────────────────────────────────────────
// Start pair
// ──────────────────────────────────────────────────────────────────────────────

fn start_stun_pair(
    bind_uri: &BindUri,
    bind_iface: &BindInterface,
    config: &StunNodeConfig,
) -> Option<IfaceStunHandle> {
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

    // WeakInterface 作为主 StunServer 的 RefIO
    let weak_iface: WeakInterface = bind_iface.borrow_weak();

    // 主端口
    let main_port = main_addr.port();

    // 地址族（IPv4 or IPv6），用于 change_address 过滤
    let main_is_ipv4 = is_ipv4_bind_uri(bind_uri);

    // 计算 change_address：
    // - 配置中有 change_address → 优先使用配置值（bootstrap 场景）
    // - 配置中无 change_address → 从 stun client agent 中动态选取（dynamic 场景）
    let change_address = config
        .change_address
        .or_else(|| compute_change_address(&iface, main_is_ipv4));

    // 派生辅助 BindUri：使用配置端口或 port=0 随机分配
    let aux_bind_uri = derive_aux_bind_uri(bind_uri, config.change_port)?;

    // 绑定辅助 iface
    let factory: Arc<dyn ProductIO> = Arc::new(DEFAULT_IO_FACTORY);
    let aux_iface: Arc<dyn IO> = Arc::from(factory.bind(aux_bind_uri));

    // 取辅助端口
    let aux_port = match aux_iface.bound_addr() {
        Ok(BoundAddr::Internet(addr)) => addr.port(),
        _ => {
            warn!(target: "stun", "Aux iface has no internet bound addr");
            return None;
        }
    };

    // 创建辅助 StunRouter（独立于 Component 体系）
    let stun_router_aux = StunRouter::new();

    // 启动辅助 iface 收包任务（仅分发 STUN 包）
    let _aux_recv_task = ReceiveAndDeliverPacket::task()
        .stun_router(stun_router_aux.clone())
        .iface_ref(aux_iface.clone())
        .spawn();

    // 主端口 StunServer：change_port 指向辅助端口
    let server_main = StunServer::new(
        weak_iface,
        stun_router_main,
        StunServerConfig::builder()
            .change_port(aux_port)
            .maybe_change_address(change_address)
            .init(),
    );

    // 辅端口 StunServer：change_port 指向主端口
    let server_aux = StunServer::new(
        aux_iface.clone(),
        stun_router_aux,
        StunServerConfig::builder()
            .change_port(main_port)
            .maybe_change_address(change_address)
            .init(),
    );

    let _server_main = server_main.spawn();
    let _server_aux = server_aux.spawn();

    Some(IfaceStunHandle {
        _aux_iface: aux_iface,
        _aux_recv_task,
        _server_main,
        _server_aux,
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
    candidates.sort_by(|a, b| (a.port(), a.ip()).cmp(&(b.port(), b.ip())));

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
    handles: &HashMap<BindUri, IfaceStunHandle>,
    publishers: &[Arc<dyn DnsPublisher + Send + Sync>],
    config: &StunNodeConfig,
) {
    let mut outer_addrs: HashSet<SocketAddr> = HashSet::new();

    // bootstrap 节点：直接用配置的 outer_address 或 bind_address 发布
    if config.is_bootstrap() {
        if let Some(addr) = config.outer_address.or(config.bind_address) {
            outer_addrs.insert(addr);
        }
    }

    // 同时收集 stun client 探测到的外网地址（两条路径互补）
    for bind_uri in handles.keys() {
        let Some(bind_iface) = find_bind_iface(listeners, bind_uri) else {
            continue;
        };
        let iface = bind_iface.borrow();
        let outers: Vec<SocketAddr> = iface
            .with_component(|clients: &StunClientsComponent| {
                clients.with_clients(|map| {
                    map.values()
                        .filter_map(|c| c.get_outer_addr()?.ok())
                        .collect::<Vec<_>>()
                })
            })
            .ok()
            .flatten()
            .unwrap_or_default();

        outer_addrs.extend(outers);
    }

    if outer_addrs.is_empty() {
        warn!(target: "stun", "No STUN endpoints to publish");
        return;
    }

    if publishers.is_empty() {
        warn!(target: "stun", "STUN endpoints found but no DNS publisher resolver available, cannot publish");
        return;
    }

    let endpoints: Vec<DnsEndpointAddr> = outer_addrs
        .iter()
        .map(|addr| match addr {
            SocketAddr::V4(a) => DnsEndpointAddr::direct_v4(*a),
            SocketAddr::V6(a) => DnsEndpointAddr::direct_v6(*a),
        })
        .collect();

    let mut hosts = HashMap::new();
    hosts.insert(STUN_DOMAIN.to_string(), endpoints);
    let packet = MdnsPacket::answer(0, &hosts).to_bytes();

    for publisher in publishers {
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
