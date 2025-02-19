use std::{net::SocketAddr, time::Duration, vec};

use qinterface::path::Endpoint;
use tokio::{net::UdpSocket, time::timeout};
use tracing::{debug, info};

// TODO: 使用配置的 DNS 服务器地址
pub const DNS_SERVER: &str = "1.12.74.4:5300";

pub async fn resolve_dns(host: &str, dns_server_addr: SocketAddr) -> std::io::Result<Endpoint> {
    let socket = UdpSocket::bind("0.0.0.0:0").await?;

    let query = format!("QUERY {}", host);

    socket.send_to(query.as_bytes(), dns_server_addr).await?;

    let mut buffer = vec![0u8; 1024];
    match timeout(Duration::from_secs(1), socket.recv_from(&mut buffer)).await? {
        Ok((len, _src)) => {
            buffer.truncate(len);
            let response = std::str::from_utf8(&buffer).unwrap();
            parse_endpoint(response)
        }
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "DNS query timed out",
        )),
    }
}

fn parse_endpoint(response: &str) -> std::io::Result<Endpoint> {
    debug!("Received DNS response: {}", response);
    let mut parts = response.split_whitespace();

    let invalid_response = |response| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid response: {}", response),
        )
    };

    let endpoint = parts.next().ok_or_else(|| invalid_response(response))?;
    let addr: Vec<&str> = endpoint.split('-').collect();
    match addr.as_slice() {
        [agent, outer] => Ok(Endpoint::Relay {
            agent: agent.parse().map_err(|_| invalid_response(response))?,
            outer: outer.parse().map_err(|_| invalid_response(response))?,
        }),
        _ => Ok(Endpoint::Direct {
            addr: endpoint.parse().map_err(|_| invalid_response(response))?,
        }),
    }
}

pub async fn report_host(
    host: &str,
    endpoint: &Endpoint,
    dns_server_addr: SocketAddr,
) -> std::io::Result<()> {
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    let report = format!("REPORT {} {}", host, ep_to_string(endpoint));
    info!("Sending DNS report: {}", report);
    socket.send_to(report.as_bytes(), dns_server_addr).await?;
    Ok(())
}

pub fn spwan_report_host_task(
    hosts: Vec<String>,
    endpoint: Vec<Endpoint>,
    dns_server_addr: SocketAddr,
) -> std::io::Result<tokio::task::JoinHandle<std::io::Result<()>>> {
    let task = tokio::spawn(async move {
        loop {
            for host in hosts.iter() {
                for ep in endpoint.iter() {
                    if let Err(e) = report_host(host, ep, dns_server_addr).await {
                        debug!("Failed to report host {}: {}", host, e);
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    });
    Ok(task)
}

fn ep_to_string(ep: &Endpoint) -> String {
    match ep {
        Endpoint::Relay { agent, outer } => format!("{}-{}", agent, outer),
        Endpoint::Direct { addr } => addr.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_resolve_dns() {
        let ep = Endpoint::Relay {
            agent: "127.0.0.1:1234".parse().unwrap(),
            outer: "127.0.0.1:5678".parse().unwrap(),
        };

        report_host("relay.example.com", &ep, DNS_SERVER.parse().unwrap())
            .await
            .unwrap();

        let endpoint = resolve_dns("relay.example.com", DNS_SERVER.parse().unwrap())
            .await
            .unwrap();
        assert_eq!(endpoint, ep);

        let ep = Endpoint::Direct {
            addr: "127.0.0.1:9000".parse().unwrap(),
        };

        report_host("direct.example.com", &ep, DNS_SERVER.parse().unwrap())
            .await
            .unwrap();

        let endpoint = resolve_dns("direct.example.com", DNS_SERVER.parse().unwrap())
            .await
            .unwrap();
        assert_eq!(endpoint, ep);
    }
}
