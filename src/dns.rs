use std::{net::SocketAddr, time::Duration, vec};

use gm_quic::EndpointAddr;
use tokio::{net::UdpSocket, time::timeout};
use tracing::{debug, info};

// TODO: 使用配置的 DNS 服务器地址
pub const DNS_SERVER: &str = "1.12.74.4:5300";

pub async fn dns_resolve(
    host: &str,
    dns_server_addr: SocketAddr,
) -> std::io::Result<Vec<EndpointAddr>> {
    let socket = UdpSocket::bind("0.0.0.0:0").await?;

    let query = format!("QUERY {}", host);

    socket.send_to(query.as_bytes(), dns_server_addr).await?;

    let mut buffer = vec![0u8; 1024];
    match timeout(Duration::from_secs(1), socket.recv_from(&mut buffer)).await? {
        Ok((len, _src)) => {
            buffer.truncate(len);
            let response = std::str::from_utf8(&buffer).unwrap();
            parse_endpoint_addrs(response)
        }
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "DNS query timed out",
        )),
    }
}

fn parse_endpoint_addrs(response: &str) -> std::io::Result<Vec<EndpointAddr>> {
    debug!("Received DNS response: {}", response);

    let invalid = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid response: {}", response),
        )
    };

    let endpoint_addrs_str = response.split_whitespace().next().ok_or_else(invalid)?;

    endpoint_addrs_str
        .split(',')
        .map(|ep| {
            Ok(match ep.split_once('-') {
                Some((agent, outer)) => EndpointAddr::Agent {
                    agent: agent.parse().map_err(|_| invalid())?,
                    outer: outer.parse().map_err(|_| invalid())?,
                },
                None => EndpointAddr::Direct {
                    addr: ep.parse().map_err(|_| invalid())?,
                },
            })
        })
        .collect()
}

pub async fn dns_publish(
    host: &str,
    endpoint_addrs: &[EndpointAddr],
    dns_server_addr: SocketAddr,
) -> std::io::Result<()> {
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    let eps = endpoint_addrs
        .iter()
        .map(dns_serialize)
        .collect::<Vec<String>>()
        .join(",");
    let report = format!("REPORT {} {}", host, eps);
    info!("Sending DNS report: {}", report);
    socket.send_to(report.as_bytes(), dns_server_addr).await?;
    Ok(())
}


fn dns_serialize(ep: &EndpointAddr) -> String {
    match ep {
        EndpointAddr::Agent { agent, outer } => format!("{}-{}", agent, outer),
        EndpointAddr::Direct { addr } => addr.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ep_to_string() {
        let reponse = "127.0.0.1:1234-127.0.0.1:5678,127.0.0.1:9000-127.0.0.1:10000,127.0.0.1:1235-127.0.0.1:5679 10";
        let eps = parse_endpoint_addrs(reponse).unwrap();
        assert_eq!(
            eps,
            [
                EndpointAddr::Agent {
                    agent: "127.0.0.1:1234".parse().unwrap(),
                    outer: "127.0.0.1:5678".parse().unwrap(),
                },
                EndpointAddr::Agent {
                    agent: "127.0.0.1:9000".parse().unwrap(),
                    outer: "127.0.0.1:10000".parse().unwrap(),
                },
                EndpointAddr::Agent {
                    agent: "127.0.0.1:1235".parse().unwrap(),
                    outer: "127.0.0.1:5679".parse().unwrap(),
                }
            ]
        );

        let response = "Not a valid response";
        assert!(parse_endpoint_addrs(response).is_err());
    }

    #[tokio::test]
    async fn test_resolve_dns() {
        let ep = EndpointAddr::Agent {
            agent: "127.0.0.1:1234".parse().unwrap(),
            outer: "127.0.0.1:5678".parse().unwrap(),
        };

        dns_publish("relay.example.com", &[ep], DNS_SERVER.parse().unwrap())
            .await
            .unwrap();

        let endpoint = dns_resolve("relay.example.com", DNS_SERVER.parse().unwrap())
            .await
            .unwrap();
        assert_eq!(endpoint, [ep]);

        let ep = EndpointAddr::Direct {
            addr: "127.0.0.1:9000".parse().unwrap(),
        };

        dns_publish("direct.example.com", &[ep], DNS_SERVER.parse().unwrap())
            .await
            .unwrap();

        let endpoint = dns_resolve("direct.example.com", DNS_SERVER.parse().unwrap())
            .await
            .unwrap();
        assert_eq!(endpoint, [ep]);

        let ep2 = EndpointAddr::Agent {
            agent: "127.0.0.1:1235".parse().unwrap(),
            outer: "127.0.0.1:5679".parse().unwrap(),
        };

        dns_publish("vec.example.com", &[ep, ep2], DNS_SERVER.parse().unwrap())
            .await
            .unwrap();
        let endpoint_addr = dns_resolve("vec.example.com", DNS_SERVER.parse().unwrap())
            .await
            .unwrap();
        assert_eq!(endpoint_addr, [ep, ep2]);
    }
}
