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

/// 管理每个 iface 上一对 StunServer（主端口 + 辅助端口）
/// 并在 FullCone 确认后将外网地址发布到 stun.genmeta.net
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
    ) -> Self {
        let _task = AbortOnDropHandle::new(tokio::spawn(async move {
            let mut handles: HashMap<BindUri, IfaceStunHandle> = HashMap::new();

            // Bootstrap 首次拉起
            reconcile_stun_servers(&listeners, &mut handles).await;
            publish_stun_endpoints(&listeners, &handles, &publishers).await;

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
                        reconcile_stun_servers(&listeners, &mut handles).await;
                        publish_stun_endpoints(&listeners, &handles, &publishers).await;
                    }
                    _ = timer.tick() => {
                        reconcile_stun_servers(&listeners, &mut handles).await;
                        publish_stun_endpoints(&listeners, &handles, &publishers).await;
                    }
                    _ = publish_timer.tick() => {
                        publish_stun_endpoints(&listeners, &handles, &publishers).await;
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
) {
    let desired = collect_fullcone_bind_uris(listeners);

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

        match start_stun_pair(bind_uri, &bind_iface) {
            Some(handle) => {
                info!(target: "stun", %bind_uri, "Started STUN server pair");
                handles.insert(bind_uri.clone(), handle);
            }
            None => {
                warn!(target: "stun", %bind_uri, "Failed to start STUN server pair, will retry");
            }
        }
    }
}

/// 收集当前所有 FullCone iface 的 BindUri
fn collect_fullcone_bind_uris(listeners: &Arc<QuicListeners>) -> HashSet<BindUri> {
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
            // for testing: treat all as fullcone
            // let is_fullcone = true;
            if is_fullcone {
                result.insert(bind_uri);
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

fn start_stun_pair(bind_uri: &BindUri, bind_iface: &BindInterface) -> Option<IfaceStunHandle> {
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

    // 计算 change_address：从活跃 stun client 的 agent 中选同地址族且不在本机 outer IP 的地址
    let change_address = compute_change_address(&iface, main_is_ipv4);

    // 派生辅助 BindUri（同 scheme/family/device，port=0）
    let aux_bind_uri = derive_aux_bind_uri(bind_uri)?;

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

/// 派生辅助 BindUri：同 scheme/family/device，port=0
fn derive_aux_bind_uri(bind_uri: &BindUri) -> Option<BindUri> {
    match bind_uri.scheme() {
        BindUriScheme::Iface => {
            let (family, device, _port) = bind_uri.as_iface_bind_uri()?;
            Some(format!("iface://{family}.{device}:0").into())
        }
        BindUriScheme::Inet => {
            let addr = bind_uri.as_inet_bind_uri()?;
            Some(SocketAddr::new(addr.ip(), 0).into())
        }
        _ => None,
    }
}

/// 从当前 iface 的 StunClientsComponent 中选取 change_address：
/// 从活跃 stun client 的 agent 中选同地址族，且 IP 不在本机 outer IP 集合中的第一个地址
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

    iface
        .with_component(|clients: &StunClientsComponent| {
            clients.with_clients(|map| {
                map.keys()
                    .copied()
                    .filter(|agent| agent.is_ipv4() == is_ipv4)
                    .find(|agent| !own_outer_ips.contains(&agent.ip()))
            })
        })
        .ok()
        .flatten()
        .flatten()
}

// ──────────────────────────────────────────────────────────────────────────────
// Publish
// ──────────────────────────────────────────────────────────────────────────────

async fn publish_stun_endpoints(
    listeners: &Arc<QuicListeners>,
    handles: &HashMap<BindUri, IfaceStunHandle>,
    publishers: &[Arc<dyn DnsPublisher + Send + Sync>],
) {
    let mut outer_addrs: HashSet<SocketAddr> = HashSet::new();

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
