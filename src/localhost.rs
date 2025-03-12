use std::{
    collections::HashMap,
    io,
    net::{SocketAddr, SocketAddrV4, SocketAddrV6},
    sync::Arc,
    time::Duration,
};

use dashmap::DashMap;
use gm_quic::{Connection, EndpointAddr, Link, Pathway};
use qconnection::traversal::NatType;
use qinterface::forward::ForwardInterface;
use qtraversal::AddressRegisty;
use tracing::{error, info, warn};

use crate::dns::dns_publish;

pub const AGENT: &str = "1.12.74.4:20002";
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
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        for (addr, name) in addr_map {
            let agent: SocketAddr = match addr.is_ipv4() {
                true => AGENT.parse().unwrap(),
                false => AGENT_V6.parse().unwrap(),
            };
            let registry = AddressRegisty::new(addr, agent);
            if let Err(e) = registry {
                warn!(
                    "init_network failed for addr {:?} {}  error {:?}",
                    addr, name, e
                );
                continue;
            }
            let registry = registry.unwrap();
            #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
            if registry.bind_device(&name).is_err() {
                warn!("init_network failed for device {}", name);
            }

            tokio::spawn({
                let tx = tx.clone();
                let localhost = self.clone();
                let registry = registry.clone();
                async move {
                    if let Ok(outer) = registry.detect_outer_addr().await {
                        info!(
                            "init_network success for outer addr {:?} local {} device {}",
                            outer, addr, name
                        );
                        localhost.0.registrys.insert(addr, registry.clone());
                        let _ = tx.send(true).await;
                    } else {
                        warn!("init_network failed for addr {:?} {}", addr, name);
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
        rx.recv().await;
        info!("LocalHost init network done.");
    }

    // TODO: remote 可能是直连
    pub async fn match_pathway(&self, remote: EndpointAddr) -> Option<(Pathway, Link)> {
        let is_v4 = remote.is_ipv4();
        let ret = self
            .0
            .registrys
            .iter()
            .find(|item| item.key().is_ipv4() == is_v4)?;
        let registry = ret.value();
        let local = EndpointAddr::Agent {
            agent: registry.agent(),
            outer: registry.outer_addr().await.unwrap(),
        };
        let pathway = Pathway::new(local, remote);
        let socket = Link::new(*ret.key(), *remote);
        Some((pathway, socket))
    }

    pub async fn relay_ep(&self) -> Vec<EndpointAddr> {
        let mut eps = Vec::new();
        for item in self.0.registrys.iter() {
            let registry = item.value();
            let agent = registry.agent();
            if let Ok(outer) = registry.outer_addr().await {
                eps.push(EndpointAddr::Agent { agent, outer });
            } else {
                warn!("get outer error, bind {} ", registry.bind_addr());
            }
        }
        eps
    }

    pub fn add_direct_address(&self, conn: Arc<Connection>) {
        let localhost = self.clone();
        tokio::spawn(async move {
            for addr in localhost.addresses() {
                let _ = conn.add_address(addr, addr, 0, NatType::RestrictedCone);
                info!("add direct loacl addr {}", addr)
            }
            for item in localhost.0.registrys.iter() {
                let registry = item.value();
                let bind = registry.bind_addr();
                if let Ok(outer) = registry.outer_addr().await {
                    if let Ok(nat_type) = registry.nat_type().await {
                        info!("add direct addr {} outer {} {:?}", bind, outer, nat_type);
                        let _ = conn.add_address(bind, outer, 1, nat_type);
                    }
                }
            }
        });
    }

    pub fn iface(&self, bind: SocketAddr) -> Option<ForwardInterface> {
        let ret = self.0.registrys.get(&bind)?;
        let registry = ret.value();
        Some(registry.iface())
    }

    pub fn addresses(&self) -> Vec<SocketAddr> {
        self.0.registrys.iter().map(|item| *item.key()).collect()
    }

    pub fn report_dns(&self, hosts: Vec<String>, dns_server: SocketAddr) {
        tokio::spawn({
            let localhost = self.clone();
            async move {
                loop {
                    let eps = localhost.relay_ep().await;
                    if !eps.is_empty() {
                        for host in &hosts {
                            if let Err(e) = dns_publish(host, &eps, dns_server).await {
                                warn!("Failed to report host {}: {}", host, e);
                            }
                        }
                    }
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
            }
        });
    }

    pub async fn resume_network(&self) -> io::Result<()> {
        let mut to_remove = Vec::new();

        for item in self.0.registrys.iter() {
            let key = item.key();
            let value = item.value();
            if value.detect_outer_addr().await.is_err() || value.detect_nat_type().await.is_err() {
                warn!("resume_network failed for addr {}", key);
                to_remove.push(*key);
            }
        }

        for key in to_remove {
            self.0.registrys.remove(&key);
        }
        Ok(())
    }

    fn scan_device(&self) -> HashMap<SocketAddr, String> {
        let mut address_map = HashMap::new();

        let ift = getifs::interfaces()
            .inspect_err(|e| {
                error!("Failed to get network interfaces: {:?}", e);
            })
            .expect("Failed to get network interfaces");
        tracing::trace!("all interfaces {:?}", ift);
        for ifi in ift {
            if let Ok(addrs) =
                ifi.ipv4_addrs_by_filter(|addr| addr.is_global() || addr.is_private())
            {
                if let Some(addr) = addrs.last() {
                    let addr = addr.addr();
                    let name = ifi.name().to_string();
                    info!("scan_device found address {} for interface {}", addr, name);
                    address_map.insert(SocketAddr::V4(SocketAddrV4::new(addr, self.0.port)), name);
                }
            }

            if let Ok(addrs) = ifi.ipv6_addrs_by_filter(|addr| addr.is_global()) {
                if let Some(addr) = addrs.last() {
                    let addr = addr.addr();
                    let name = ifi.name().to_string();
                    info!("scan_device found address {} for interface {}", addr, name);
                    address_map.insert(
                        SocketAddr::V6(SocketAddrV6::new(addr, self.0.port, 0, 0)),
                        name,
                    );
                }
            }
        }
        address_map
    }
}
