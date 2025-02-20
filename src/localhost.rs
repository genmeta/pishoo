use std::{
    collections::HashMap,
    io,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use dashmap::DashMap;
use gm_quic::{Connection, Endpoint, Pathway, Socket};
use qinterface::forward::ForwardInterface;
use qtraversal::AddressRegisty;
use tracing::{info, warn};

pub const AGENT: &str = "1.12.74.4:20002";
// todo: agent_v6
pub const AGENT_V6: &str = "[2402:4e00:c011:1700:8624:7e0:5c9a:2]:20002";

struct LocalHost {
    port: u16,
    registrys: DashMap<SocketAddr, AddressRegisty>,
}

impl LocalHost {
    fn new(port: u16) -> Self {
        Self {
            port,
            registrys: DashMap::new(),
        }
    }
}

#[derive(Clone)]
pub struct ArcLocalHost(Arc<LocalHost>);

impl ArcLocalHost {
    pub fn new(port: u16) -> Self {
        Self(Arc::new(LocalHost::new(port)))
    }

    pub async fn init_network(&self) {
        let addr_map = self.scan_device();
        for (addr, name) in addr_map {
            let agent: SocketAddr = match addr.is_ipv4() {
                true => AGENT.parse().unwrap(),
                false => AGENT_V6.parse().unwrap(),
            };
            let registry = AddressRegisty::new(addr, agent);
            if registry.is_err() {
                warn!("init_network failed for addr {:?} {}", addr, name);
                continue;
            }
            let registry = registry.unwrap();
            #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
            if registry.bind_device(&name).is_err() {
                warn!("init_network failed for device {}", name);
            }

            self.0.registrys.insert(addr, registry.clone());
            tokio::spawn({
                let localhost = self.clone();
                let registry = registry.clone();
                async move {
                    // 探测外网地址错误，移除
                    if let Ok(outer) = registry.detect_outer_addr().await {
                        info!(
                            "init_network failed for outer addr {:?} local {} device {}",
                            outer, addr, name
                        );
                    } else {
                        warn!("init_network failed for addr {:?} {}", addr, name);
                        localhost.0.registrys.remove(&addr);
                        return;
                    }
                    let nat_type = registry.detect_nat_type().await;
                    info!(
                        "init_network found nat_type {:?} for addr {:?} local {} device {}",
                        nat_type, addr, addr, name
                    );
                    let _ = registry.keep_alive(Duration::from_secs(30));
                }
            });
        }
        info!("LocalHost init network done.");
    }

    // TODO: remote 可能是直连
    pub async fn match_pathway(&self, remote: Endpoint) -> Option<(Pathway, Socket)> {
        let is_v4 = remote.is_ipv4();
        let ret = self
            .0
            .registrys
            .iter()
            .find(|item| item.key().is_ipv4() == is_v4)?;
        let registry = ret.value();
        let local = Endpoint::Relay {
            agent: registry.agent(),
            outer: registry.outer_addr().await.unwrap(),
        };
        let pathway = Pathway::new(local, remote);
        let socket = Socket::new(*ret.key(), *remote);
        Some((pathway, socket))
    }

    pub async fn relay_ep(&self) -> Vec<Endpoint> {
        let mut eps = Vec::new();
        for item in self.0.registrys.iter() {
            let registry = item.value();
            let agent = registry.agent();
            let outer = registry.outer_addr().await.unwrap();
            eps.push(Endpoint::Relay { agent, outer });
        }
        eps
    }

    pub async fn add_direct_address(&self, conn: Arc<Connection>) {
        for item in self.0.registrys.iter() {
            let registry = item.value();
            let bind = registry.bind_addr();
            if let Ok(outer) = registry.outer_addr().await {
                if let Ok(nat_type) = registry.detect_nat_type().await {
                    let _ = conn.add_address(bind, outer, 1, nat_type);
                }
            }
        }
    }

    pub fn iface(&self, bind: SocketAddr) -> Option<ForwardInterface> {
        let ret = self.0.registrys.get(&bind)?;
        let registry = ret.value();
        Some(registry.iface())
    }

    pub fn addresses(&self) -> Vec<SocketAddr> {
        self.0.registrys.iter().map(|item| *item.key()).collect()
    }

    pub async fn resume_network(&self) -> io::Result<()> {
        for item in self.0.registrys.iter() {
            item.value().detect_outer_addr().await?;
            item.value().detect_nat_type().await?;
        }
        Ok(())
    }

    fn scan_device(&self) -> HashMap<SocketAddr, String> {
        let mut address_map = HashMap::new();

        let interfaces = pnet::datalink::interfaces();
        for iface in interfaces {
            if iface.is_up() && iface.is_running() && !iface.is_loopback() {
                for ip in iface.ips {
                    if let IpAddr::V6(v6_ip) = ip.ip() {
                        // skip link-local addresses
                        if !v6_ip.segments()[0] & 0xffc0 == 0xfe80 {
                            let socket_addr = SocketAddr::new(ip.ip(), self.0.port);
                            info!(
                                "scan_device found address {} for interface {}",
                                socket_addr, iface.name
                            );
                            address_map.insert(socket_addr, iface.name.clone());
                        }
                    } else {
                        let socket_addr = SocketAddr::new(ip.ip(), self.0.port);
                        info!(
                            "scan_device found address {} for interface {}",
                            socket_addr, iface.name
                        );
                        address_map.insert(socket_addr, iface.name.clone());
                    }
                }
            }
        }
        address_map
    }
}
