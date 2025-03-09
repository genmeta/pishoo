use std::{net::SocketAddr, time::Duration, vec};

use async_trait::async_trait;
use futures::io;
use gm_quic::EndpointAddr;
use tokio::{net::UdpSocket, time::timeout};
use tracing::{debug, info, warn};

use crate::{Resolver, localhost::ArcLocalHost};

#[derive(Clone, Copy)]
pub struct Dns(SocketAddr);

impl Dns {
    pub fn new(server: SocketAddr) -> Dns {
        Dns(server)
    }

    pub fn spwan_publish(&self, names: Vec<String>, localhost: ArcLocalHost) {
        tokio::spawn({
            let dns = *self;
            async move {
                loop {
                    let eps = localhost.relay_addr().await;
                    if !eps.is_empty() {
                        for name in names.iter() {
                            if let Err(e) = dns.publish(name, &eps).await {
                                warn!("Failed to report host {}: {}", name, e);
                            }
                        }
                    }
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
            }
        });
    }
}

#[async_trait]
impl Resolver for Dns {
    async fn publish(&self, name: &str, addresses: &[EndpointAddr]) -> io::Result<()> {
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        let report = format!("REPORT {} {}", name, dns_serialize(addresses));
        info!("Sending DNS report: {}", report);
        socket.send_to(report.as_bytes(), self.0).await?;
        Ok(())
    }

    async fn look_up(&self, name: &str) -> io::Result<Vec<EndpointAddr>> {
        let query = format!("QUERY {}", name);
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        let mut buffer = vec![0u8; 1024];
        const RETRY: u8 = 3;
        for i in 0..RETRY {
            socket.send_to(query.as_bytes(), self.0).await?;
            match timeout(Duration::from_secs(1), socket.recv_from(&mut buffer)).await? {
                Ok((len, _src)) => {
                    buffer.truncate(len);
                    let response = std::str::from_utf8(&buffer).unwrap();
                    return dns_deserialize(response);
                }
                Err(_) => {
                    warn!("dns timeout retry {}", i + 1)
                }
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "DNS query timed out",
        ))
    }
}

#[inline]
fn dns_deserialize(response: &str) -> io::Result<Vec<EndpointAddr>> {
    debug!("Received DNS response: {}", response);

    let invalid_data = |msg: String| io::Error::new(io::ErrorKind::InvalidData, msg);
    let (addrs, _ttl) = response
        .split_once(' ')
        .ok_or_else(|| invalid_data(format!("invalid format: {}", response)))?;

    addrs
        .split(',')
        .map(|s| {
            s.trim().parse().map_err(|e| {
                warn!("Invalid endpoint address '{}': {}", s, e);
                invalid_data(format!("address parse failed: {}", e))
            })
        })
        .collect()
}

#[inline]
fn dns_serialize(addresses: &[EndpointAddr]) -> String {
    addresses
        .iter()
        .map(|ep| match ep {
            EndpointAddr::Agent { agent, outer } => format!("{}-{}", agent, outer),
            EndpointAddr::Direct { addr } => addr.to_string(),
        })
        .collect::<Vec<String>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ep_to_string() {
        let reponse = "127.0.0.1:1234-127.0.0.1:5678,127.0.0.1:9000-127.0.0.1:10000,127.0.0.1:1235-127.0.0.1:5679 10";
        let eps = dns_deserialize(reponse).unwrap();
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
        assert!(dns_deserialize(response).is_err());
    }

    #[tokio::test]
    async fn test_resolve_dns() {
        let ep = EndpointAddr::Agent {
            agent: "127.0.0.1:1234".parse().unwrap(),
            outer: "127.0.0.1:5678".parse().unwrap(),
        };

        let dns_server = Dns::new("1.12.74.4:5300".parse().unwrap());
        dns_server
            .publish("relay.example.com", &[ep])
            .await
            .unwrap();

        let endpoint = dns_server.look_up("relay.example.com").await.unwrap();
        assert_eq!(endpoint, [ep]);

        let ep = EndpointAddr::Direct {
            addr: "127.0.0.1:9000".parse().unwrap(),
        };

        dns_server
            .publish("direct.example.com", &[ep])
            .await
            .unwrap();

        let endpoint = dns_server.look_up("direct.example.com").await.unwrap();
        assert_eq!(endpoint, [ep]);

        let ep2 = EndpointAddr::Agent {
            agent: "127.0.0.1:1235".parse().unwrap(),
            outer: "127.0.0.1:5679".parse().unwrap(),
        };

        dns_server
            .publish("vec.example.com", &[ep, ep2])
            .await
            .unwrap();

        let endpoint_addr = dns_server.look_up("vec.example.com").await.unwrap();
        assert_eq!(endpoint_addr, [ep, ep2]);
    }
}
