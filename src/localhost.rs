use std::{collections::HashMap, io, net::SocketAddr, sync::Arc};

use gm_quic::{ProductQuicInterface, QuicInterface};
use qtraversal::iface::UdpQuicInterface;
use tracing::info;

pub struct TraversalFactory {
    agents: Vec<SocketAddr>,
    /// ip => device name
    devices: HashMap<String, String>,
}

impl TraversalFactory {
    pub fn with(agents: &[SocketAddr]) -> Self {
        let devices = scan_device();
        Self {
            agents: agents.to_vec(),
            devices,
        }
    }

    pub fn devices(&self) -> &HashMap<String, String> {
        &self.devices
    }
}

impl ProductQuicInterface for TraversalFactory {
    fn bind(&self, bind: SocketAddr) -> io::Result<Arc<dyn QuicInterface>> {
        let agent = if let Some(agent) = self
            .agents
            .iter()
            .find(|addr| addr.is_ipv4() == bind.is_ipv4())
        {
            *agent
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "bind address must be ipv4 or ipv6",
            ));
        };

        let iface = UdpQuicInterface::new(bind, agent);

        let iface = if let Some(device) = self.devices.get(bind.ip().to_string().as_str()) {
            iface.inspect(|iface| {
                iface.bind_device(device).unwrap();
            })
        } else {
            iface
        };

        iface.map(|iface| Arc::new(iface) as Arc<dyn QuicInterface>)
    }
}

// TODO 分类 ADDRESS 返回 IP, name
#[cfg(target_os = "android")]
fn scan_device() -> HashMap<String, String> {
    use std::net::IpAddr;

    let mut addresses = HashMap::new();

    let interfaces = pnet::datalink::interfaces();
    for iface in interfaces {
        if iface.is_up() && !iface.is_loopback() {
            if let Some(ip) = iface.ips.iter().filter(|ip| ip.is_ipv4()).next_back() {
                info!(
                    "scan_device found address {} for interface {}",
                    ip, iface.name
                );
                addresses.insert(ip.ip().to_string(), iface.name.clone());
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
                info!(
                    "scan_device found address {} for interface {}",
                    ip, iface.name
                );
                addresses.insert(ip.ip().to_string(), iface.name.clone());
            }
        }
    }
    addresses
}

#[cfg(not(target_os = "android"))]
pub(crate) fn scan_device() -> HashMap<String, String> {
    use tracing::error;

    let mut addresses = HashMap::new();

    let ift = getifs::interfaces()
        .inspect_err(|e| {
            error!("Failed to get network interfaces: {:?}", e);
        })
        .expect("Failed to get network interfaces");
    tracing::debug!("all interfaces {:?}", ift);
    for ifi in ift {
        if let Ok(addrs) = ifi.ipv4_addrs_by_filter(|addr| addr.is_global() || addr.is_private()) {
            if let Some(addr) = addrs.last() {
                let addr = addr.addr();
                let name = ifi.name().to_string();
                info!("scan_device found address {} for interface {}", addr, name);
                addresses.insert(addr.to_string(), name);
            }
        }

        if let Ok(addrs) = ifi.ipv6_addrs_by_filter(|addr| addr.is_global()) {
            if let Some(addr) = addrs.last() {
                let addr = addr.addr();
                let name = ifi.name().to_string();
                info!("scan_device found address {} for interface {}", addr, name);
                addresses.insert(addr.to_string(), name);
            }
        }
    }

    addresses
}
