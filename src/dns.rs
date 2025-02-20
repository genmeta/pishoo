use std::{net::SocketAddr, time::Duration, vec};

use qinterface::path::Endpoint;
use tokio::{net::UdpSocket, time::timeout};
use tracing::{debug, info};

// TODO: 使用配置的 DNS 服务器地址
pub const DNS_SERVER: &str = "1.12.74.4:5300";

pub async fn resolve_dns(
    host: &str,
    dns_server_addr: SocketAddr,
) -> std::io::Result<Vec<Endpoint>> {
    let socket = UdpSocket::bind("0.0.0.0:0").await?;

    let query = format!("QUERY {}", host);

    socket.send_to(query.as_bytes(), dns_server_addr).await?;

    let mut buffer = vec![0u8; 1024];
    match timeout(Duration::from_secs(1), socket.recv_from(&mut buffer)).await? {
        Ok((len, _src)) => {
            buffer.truncate(len);
            let response = std::str::from_utf8(&buffer).unwrap();
            parse_endpoints(response)
        }
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "DNS query timed out",
        )),
    }
}

fn parse_endpoints(response: &str) -> std::io::Result<Vec<Endpoint>> {
    debug!("Received DNS response: {}", response);

    let invalid = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid response: {}", response),
        )
    };

    let endpoints_str = response.split_whitespace().next().ok_or_else(invalid)?;

    endpoints_str
        .split(',')
        .map(|ep| {
            Ok(match ep.split_once('-') {
                Some((agent, outer)) => Endpoint::Relay {
                    agent: agent.parse().map_err(|_| invalid())?,
                    outer: outer.parse().map_err(|_| invalid())?,
                },
                None => Endpoint::Direct {
                    addr: ep.parse().map_err(|_| invalid())?,
                },
            })
        })
        .collect()
}

pub async fn report_host(
    host: &str,
    endpoints: &[Endpoint],
    dns_server_addr: SocketAddr,
) -> std::io::Result<()> {
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    let eps = endpoints
        .iter()
        .map(ep_to_string)
        .collect::<Vec<String>>()
        .join(",");
    let report = format!("REPORT {} {}", host, eps);
    info!("Sending DNS report: {}", report);
    socket.send_to(report.as_bytes(), dns_server_addr).await?;
    Ok(())
}

pub fn spwan_report_host_task(
    hosts: Vec<String>,
    endpoints: Vec<Endpoint>,
    dns_server_addr: SocketAddr,
) -> std::io::Result<tokio::task::JoinHandle<std::io::Result<()>>> {
    let task = tokio::spawn(async move {
        loop {
            for host in hosts.iter() {
                if let Err(e) = report_host(host, &endpoints, dns_server_addr).await {
                    debug!("Failed to report host {}: {}", host, e);
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

    #[test]
    fn test_ep_to_string() {
        let reponse = "127.0.0.1:1234-127.0.0.1:5678,127.0.0.1:9000-127.0.0.1:10000,127.0.0.1:1235-127.0.0.1:5679 10";
        let eps = parse_endpoints(reponse).unwrap();
        assert_eq!(eps, [
            Endpoint::Relay {
                agent: "127.0.0.1:1234".parse().unwrap(),
                outer: "127.0.0.1:5678".parse().unwrap(),
            },
            Endpoint::Relay {
                agent: "127.0.0.1:9000".parse().unwrap(),
                outer: "127.0.0.1:10000".parse().unwrap(),
            },
            Endpoint::Relay {
                agent: "127.0.0.1:1235".parse().unwrap(),
                outer: "127.0.0.1:5679".parse().unwrap(),
            }
        ]);

        let response = "Not a valid response";
        assert!(parse_endpoints(response).is_err());
    }

    #[tokio::test]
    #[ignore]
    async fn test_resolve_dns() {
        let ep = Endpoint::Relay {
            agent: "127.0.0.1:1234".parse().unwrap(),
            outer: "127.0.0.1:5678".parse().unwrap(),
        };

        report_host("relay.example.com", &[ep], DNS_SERVER.parse().unwrap())
            .await
            .unwrap();

        let endpoint = resolve_dns("relay.example.com", DNS_SERVER.parse().unwrap())
            .await
            .unwrap();
        assert_eq!(endpoint, [ep]);

        let ep = Endpoint::Direct {
            addr: "127.0.0.1:9000".parse().unwrap(),
        };

        report_host("direct.example.com", &[ep], DNS_SERVER.parse().unwrap())
            .await
            .unwrap();

        let endpoint = resolve_dns("direct.example.com", DNS_SERVER.parse().unwrap())
            .await
            .unwrap();
        assert_eq!(endpoint, [ep]);

        let ep2 = Endpoint::Relay {
            agent: "127.0.0.1:1235".parse().unwrap(),
            outer: "127.0.0.1:5679".parse().unwrap(),
        };

        report_host("vec.example.com", &[ep, ep2], DNS_SERVER.parse().unwrap())
            .await
            .unwrap();
        let endpoint = resolve_dns("vec.example.com", DNS_SERVER.parse().unwrap())
            .await
            .unwrap();
        assert_eq!(endpoint, [ep, ep2]);
    }
}
