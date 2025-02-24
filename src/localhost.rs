use std::{
    collections::HashMap,
    io,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use dashmap::DashMap;
use gm_quic::{Connection, EndpointAddr, Link, Pathway};
use qinterface::forward::ForwardInterface;
use qtraversal::AddressRegisty;
use tracing::{info, warn};

use crate::dns::{DNS_SERVER, report_host};

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

    pub async fn add_direct_address(&self, conn: Arc<Connection>) {
        for item in self.0.registrys.iter() {
            let registry = item.value();
            let bind = registry.bind_addr();
            if let Ok(outer) = registry.outer_addr().await {
                if let Ok(nat_type) = registry.nat_type().await {
                    info!("add direct addr {} {} {:?}", bind, outer, nat_type);
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

    pub fn report_dns(&self, hosts: Vec<String>) {
        let dns_server = DNS_SERVER.parse().unwrap();
        tokio::spawn({
            let localhost = self.clone();
            async move {
                loop {
                    let eps = localhost.relay_ep().await;
                    if !eps.is_empty() {
                        for host in &hosts {
                            if let Err(e) = report_host(host, &eps, dns_server).await {
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

    #[cfg(target_os = "windows")]
    fn scan_device(&self) -> HashMap<SocketAddr, String> {
        use std::net::{Ipv4Addr, Ipv6Addr};
        let addr4 = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), self.0.port);
        let addr6 = SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), self.0.port);
        let mut address_map = HashMap::new();

        address_map.insert(addr4, "eth0".to_string());
        address_map.insert(addr6, "eth0".to_string());
        address_map
    }

    #[cfg(not(target_os = "windows"))]
    fn scan_device(&self) -> HashMap<SocketAddr, String> {
        let mut address_map = HashMap::new();

        let interfaces = pnet::datalink::interfaces();
        tracing::trace!("all interfaces {:?}", interfaces);
        let mut has_v6 = false;
        for iface in interfaces {
            if iface.is_up() && !iface.is_loopback() {
                for ip in iface.ips {
                    if let IpAddr::V6(v6_ip) = ip.ip() {
                        // skip link-local addresses
                        if (v6_ip.segments()[0] & 0xffc0) != 0xfe80 && !has_v6{
                            let socket_addr = SocketAddr::new(ip.ip(), self.0.port);
                            info!(
                                "scan_device found address {} for interface {}",
                                socket_addr, iface.name
                            );
                            address_map.insert(socket_addr, iface.name.clone());
                            // TODO: 多个 v6 地址直连可能会有问题？
                            has_v6 = true;
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
