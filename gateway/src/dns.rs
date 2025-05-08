use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    sync::{Arc, LazyLock},
    time::Duration,
    vec,
};

use async_trait::async_trait;
use gm_quic::EndpointAddr;
use parking_lot::Mutex;
use qtraversal::MAPPED_ENDPOINTS;
use tokio::{net::UdpSocket, task::JoinSet, time::timeout};
use tracing::{debug, error, info, warn};

use crate::Resolver;

/// 记录全局的 需要发布的域名, 与其绑定的 binds
static RECORDS: LazyLock<Mutex<RecordsList>> = LazyLock::new(Default::default);

// 新增类型定义
type RecordsList = Vec<Record>;

struct Record {
    name: String,
    resolver: UdpResolver,
    binds: Vec<SocketAddr>,
}

#[derive(Clone, Default)]
pub struct Dns {
    mapped_endpoints: Arc<Mutex<HashMap<SocketAddr, EndpointAddr>>>,
}

impl Dns {
    pub fn spawn_publish(&self) {
        // 定时任务, 监听 MAPPED_ENDPOINTS 队列, 更新 binds => EndpointAddr, 并在更新时 通知 DNS 服务器
        tokio::spawn({
            let dns = self.clone();
            async move {
                while let Some((bind, endpoint)) = MAPPED_ENDPOINTS.pop().await {
                    dns.mapped_endpoints.lock().insert(bind, endpoint);
                    match dns.publish_records().await {
                        Ok(()) => {
                            info!("[DNS] Published records successfully");
                        }
                        Err(e) => {
                            error!("[DNS] Failed to publish records: {}", e);
                        }
                    };
                }
            }
        });

        tokio::spawn({
            let dns = self.clone();
            let mut interval = tokio::time::interval(Duration::from_secs(10));
            async move {
                loop {
                    match dns.publish_records().await {
                        Ok(()) => {
                            info!("[DNS] Published records successfully");
                        }
                        Err(e) => {
                            error!("[DNS] Failed to publish records: {}", e);
                        }
                    };
                    interval.tick().await;
                }
            }
        });
    }

    /// 发布所有需要发布的域名到 DNS 服务器
    async fn publish_records(&self) -> io::Result<()> {
        let mut handler = JoinSet::new();
        {
            let records_guard = RECORDS.lock();
            let mapped_endpoints_guard = self.mapped_endpoints.lock();
            for record in records_guard.iter() {
                let mut eps = vec![];
                for bind in &record.binds {
                    if let Some(ep) = mapped_endpoints_guard.get(bind) {
                        eps.push(*ep);
                    } else {
                        warn!("[DNS] No endpoint found for bind: {}", bind);
                    }
                }
                handler.spawn({
                    let server_name = record.name.to_string();
                    let resolver = record.resolver;
                    async move { resolver.publish(&server_name, &eps).await }
                });
            }
        };

        handler.join_all().await;

        Ok(())
    }

    pub fn add_record(name: String, resolver: SocketAddr, binds: Vec<SocketAddr>) {
        let mut records = RECORDS.lock();
        records.push(Record {
            name,
            resolver: UdpResolver::new(resolver),
            binds,
        });
    }
}

#[derive(Debug, Clone, Copy)]
pub struct UdpResolver {
    resolver: SocketAddr,
}

impl UdpResolver {
    pub fn new(resolver: SocketAddr) -> Self {
        UdpResolver { resolver }
    }
}

#[async_trait]
impl Resolver for UdpResolver {
    async fn publish(&self, name: &str, addresses: &[EndpointAddr]) -> io::Result<()> {
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        let report = format!("REPORT {} {}", name, dns_serialize(addresses));
        debug!("Sending DNS report: {}", report);
        socket.send_to(report.as_bytes(), self.resolver).await?;
        Ok(())
    }

    async fn look_up(&self, name: &str) -> io::Result<Vec<EndpointAddr>> {
        let query = format!("QUERY {name}");
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        let mut buffer = vec![0u8; 1024];
        const RETRY: u8 = 3;
        for i in 0..RETRY {
            socket.send_to(query.as_bytes(), self.resolver).await?;
            match timeout(Duration::from_secs(1), socket.recv_from(&mut buffer)).await? {
                Ok((len, _src)) => {
                    buffer.truncate(len);
                    let response = std::str::from_utf8(&buffer).unwrap();
                    debug!("[DNS] Received DNS response: {}", response);
                    return dns_deserialize(response);
                }
                Err(_) => {
                    warn!("[DNS] timeout retry {}", i + 1)
                }
            }
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
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
        .ok_or_else(|| invalid_data(format!("invalid format: {response}")))?;

    addrs
        .split(',')
        .map(|s| {
            s.trim().parse().map_err(|e| {
                warn!("Invalid endpoint address '{}': {}", s, e);
                invalid_data(format!("address parse failed: {e}"))
            })
        })
        .collect()
}

#[inline]
fn dns_serialize(addresses: &[EndpointAddr]) -> String {
    addresses
        .iter()
        .map(|ep| match ep {
            EndpointAddr::Agent { agent, outer } => format!("{agent}-{outer}"),
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

        let dns_server = UdpResolver::new("1.12.74.4:5300".parse().unwrap());
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
