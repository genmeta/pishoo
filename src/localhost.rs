use std::{
    io,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use dashmap::DashMap;
use gm_quic::{Connection, EndpointAddr, Link, Pathway};
use qconnection::traversal::NatType;
use qinterface::forward::ForwardInterface;
use qtraversal::AddressRegisty;
use tracing::{info, warn};

pub const AGENT: &str = "1.12.74.4:20002";
pub const AGENT_V6: &str = "[2402:4e00:c011:1700:8624:7e0:5c9a:2]:20002";

struct LocalHost {
    port: u16,
    registrys: DashMap<SocketAddr, AddressRegisty>,
    bind_address: DashMap<SocketAddr, String>,
}

impl LocalHost {
    fn new(port: u16) -> Self {
        Self {
            port,
            registrys: DashMap::new(),
            bind_address: DashMap::new(),
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
        self.scan_device();
        for item in self.0.bind_address.iter() {
            let addr = *item.key();
            let name = item.value().clone();
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

            self.0.registrys.insert(addr, registry.clone());
            tokio::spawn({
                let localhost = self.clone();
                async move {
                    if let Ok(outer) = registry.detect_outer_addr().await {
                        info!(
                            "init_network success for outer addr {:?} local {} device {}",
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

    pub async fn relay_addr(&self) -> Vec<EndpointAddr> {
        let mut eps: Vec<EndpointAddr> = Vec::new();
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
        self.0.bind_address.iter().map(|item| *item.key()).collect()
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
    fn scan_device(&self) {
        use std::net::{Ipv4Addr, Ipv6Addr};
        let addr4 = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), self.0.port);
        let addr6 = SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), self.0.port);
        self.0.bind_address.insert(addr4, "eth0".to_string());
        self.0.bind_address.insert(addr6, "eth0".to_string());
    }

    #[cfg(not(target_os = "windows"))]
    fn scan_device(&self) {
        let interfaces = pnet::datalink::interfaces();
        for iface in interfaces {
            if iface.is_up() && !iface.is_loopback() {
                if let Some(ip) = iface.ips.iter().filter(|ip| ip.is_ipv4()).next_back() {
                    let socket_addr = SocketAddr::new(ip.ip(), self.0.port);
                    info!(
                        "scan_device found address {} for interface {}",
                        socket_addr, iface.name
                    );
                    self.0.bind_address.insert(socket_addr, iface.name.clone());
                }

                if let Some(ip) = iface
                    .ips
                    .iter()
                    .filter(|ip| {
                        ip.is_ipv6() && {
                            if let IpAddr::V6(v6_ip) = ip.ip() {
                                (v6_ip.segments()[0] & 0xffc0) != 0xfe80
                            } else {
                                false
                            }
                        }
                    })
                    .next_back()
                {
                    let socket_addr = SocketAddr::new(ip.ip(), self.0.port);
                    info!(
                        "scan_device found address {} for interface {}",
                        socket_addr, iface.name
                    );
                    self.0.bind_address.insert(socket_addr, iface.name.clone());
                }
            }
        }
    }
}
