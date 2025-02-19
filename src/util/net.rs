use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6, ToSocketAddrs, UdpSocket};

pub type Port = u16;

// Try to bind to a socket using UDP
fn test_bind_udp<A: ToSocketAddrs>(addr: A) -> Option<Port> {
    Some(UdpSocket::bind(addr).ok()?.local_addr().ok()?.port())
}

/// Check if a port is free on UDP
pub fn is_free_udp(port: Port) -> bool {
    let ipv4 = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port);
    let ipv6 = SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, port, 0, 0);

    test_bind_udp(ipv6).is_some() && test_bind_udp(ipv4).is_some()
}

/// Asks the OS for a free port
fn ask_free_udp_port() -> Option<Port> {
    let ipv4 = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0);
    let ipv6 = SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0);

    test_bind_udp(ipv6).or_else(|| test_bind_udp(ipv4))
}

pub fn pick_unused_udp_port() -> Option<Port> {
    let ports = fastrand::choose_multiple(15000..25000, 50);

    // Try random port first
    for port in ports {
        if is_free_udp(port) {
            return Some(port);
        }
    }

    // Ask the OS for a port
    for _ in 0..10 {
        if let Some(port) = ask_free_udp_port() {
            return Some(port);
        }
    }

    // Give up
    None
}

#[cfg(test)]
mod tests {
    use super::pick_unused_udp_port;

    #[test]
    fn it_works() {
        assert!(pick_unused_udp_port().is_some());
    }
}
