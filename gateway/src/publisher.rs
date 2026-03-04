use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

use futures::{StreamExt, stream::FuturesUnordered};
use gm_quic::{
    prelude::{BindUri, BoundAddr, IO, QuicListeners},
    qbase::net::addr::SocketEndpointAddr,
    qdns::Publish as DnsPublisher,
    qinterface::{BindInterface, component::location::Locations},
    qtraversal::nat::client::StunClientsComponent,
};
use gmdns::{
    MdnsPacket, mdns::Mdns, parser::record::endpoint::EndpointAddr as DnsEndpointAddr,
    resolvers::MdnsResolver,
};
use rustls::{SignatureScheme, sign::SigningKey};
use snafu::Report;
use tokio::time::{self, MissedTickBehavior, interval};
use tokio_util::task::AbortOnDropHandle;

#[allow(dead_code)]
pub const HTTP_DNS_SERVER: &str = "https://dns.genmeta.net/";
pub const H3_DNS_SERVER: &str = "https://localhost:4433";
pub const MDNS_SERVICE: &str = "_genmeta.local";

pub struct Publisher {
    _task: AbortOnDropHandle<()>,
}

pub type Resolvers = Vec<Arc<dyn DnsPublisher + Send + Sync>>;

#[derive(Clone)]
pub struct ServerConfig {
    pub resolvers: Resolvers,
    pub server_id: u8,
    pub signing_key: Option<(Arc<dyn SigningKey>, SignatureScheme)>,
}

impl ServerConfig {
    fn sign_endpoint(&self, ep: &mut DnsEndpointAddr) {
        ep.set_main(self.server_id == MAIN_SERVER_ID);
        ep.set_sequence(self.server_id as u64);
        if let Some((key, scheme)) = &self.signing_key
            && let Err(e) = ep.sign_with(key.as_ref(), *scheme)
        {
            tracing::warn!(target: "dns", "Failed to sign endpoint: {e}");
        }
    }
}

fn ensure_mdns_resolver(
    bind_uri: &BindUri,
    bind_iface: &BindInterface,
) -> Option<(MdnsResolver, SocketAddr)> {
    let iface = bind_iface.borrow();

    let (_, device, _) = bind_uri.as_iface_bind_uri()?;
    let Ok(BoundAddr::Internet(local_addr)) = iface.bound_addr() else {
        return None;
    };

    if let Ok(Some(mdns)) = bind_iface
        .borrow()
        .with_component(|mdns: &Mdns| mdns.clone())
    {
        return Some((mdns, local_addr));
    }

    match Mdns::new(MDNS_SERVICE, local_addr.ip(), device) {
        Ok(mdns) => {
            let resolver = mdns.clone();

            bind_iface.with_components_mut(|components, _| {
                components.init_with(move || mdns);
            });

            Some((resolver, local_addr))
        }
        Err(e) => {
            tracing::warn!(target: "dns", "Mdns::new failed for {bind_uri} addr={local_addr} device={device}: {e}");
            None
        }
    }
}

pub const DNS_PUBLISH_INTERVAL: Duration = Duration::from_secs(10);
const MAIN_SERVER_ID: u8 = 0;

async fn publish_single_mdns(
    bind_uri: &BindUri,
    bind_iface: &BindInterface,
    server_name: &str,
    config: &ServerConfig,
) {
    if let Some((resolver, addr)) = ensure_mdns_resolver(bind_uri, bind_iface) {
        let mut ep = match addr {
            SocketAddr::V4(addr) => DnsEndpointAddr::direct_v4(addr),
            SocketAddr::V6(addr) => DnsEndpointAddr::direct_v6(addr),
        };
        config.sign_endpoint(&mut ep);

        let mut hosts = std::collections::HashMap::new();
        hosts.insert(server_name.to_string(), vec![ep]);
        let packet = MdnsPacket::answer(0, &hosts).to_bytes();

        if let Err(error) = resolver.publish(server_name, &packet).await {
            tracing::error!(
                target: "dns",
                "Resolve `{resolver}` publish dns failed: {}",
                Report::from_error(error)
            );
        } else {
            tracing::trace!(target: "dns", %bind_uri, %addr, "Publishing local address to mDNS");
        }
    }
}

async fn publish_resolvers(
    server_name: &str,
    config: &ServerConfig,
    interfaces: impl Iterator<Item = (&BindUri, &BindInterface)>,
) {
    let mut endpoints = vec![];
    for (_, bind_iface) in interfaces {
        for sock_ep in bind_iface
            .borrow()
            .with_component(|clients: &StunClientsComponent| {
                clients.with_clients(|clients| {
                    clients
                        .values()
                        .filter_map(|client| {
                            let outer = client.get_outer_addr()?.ok()?;
                            Some(SocketEndpointAddr::with_agent(client.agent_addr(), outer))
                        })
                        .collect::<Vec<_>>()
                })
            })
            .ok()
            .flatten()
            .unwrap_or_default()
        {
            if let Ok(mut ep) = DnsEndpointAddr::try_from(sock_ep) {
                config.sign_endpoint(&mut ep);
                endpoints.push(ep);
            }
        }
    }

    if endpoints.is_empty() {
        tracing::warn!(target: "dns", server_name, "No endpoints to publish for this server");
        return;
    }

    tracing::debug!(target: "dns", server_name, server_id = config.server_id, count = endpoints.len(), "Publishing endpoints");

    let mut hosts = std::collections::HashMap::new();
    hosts.insert(server_name.to_string(), endpoints);
    let packet = MdnsPacket::answer(0, &hosts).to_bytes();

    for resolver in &config.resolvers {
        if let Err(error) = resolver.publish(server_name, &packet).await {
            tracing::error!(
                target: "dns",
                "Resolver `{resolver}` publish dns failed: {}",
                Report::from_error(error)
            );
        }
    }
}

async fn publish_once(listeners: &Arc<QuicListeners>, resolvers: &HashMap<String, ServerConfig>) {
    listeners
        .servers()
        .into_iter()
        .filter_map(|name| {
            let ifaces = listeners.get_server(&name)?.bind_interfaces();
            let config = resolvers.get(&name)?;
            Some((name, ifaces, config))
        })
        .map(|(name, ifaces, config)| async move {
            // MDNS 和 H3 DNS 发布并行执行，避免 H3 DNS 卡住时阻塞 MDNS
            let mdns_name = name.clone();
            let mdns_future = async {
                ifaces
                    .iter()
                    .map(|(uri, iface)| publish_single_mdns(uri, iface, &mdns_name, config))
                    .collect::<FuturesUnordered<_>>()
                    .collect::<()>()
                    .await;
            };

            let resolvers_future = publish_resolvers(&name, config, ifaces.iter());

            tokio::join!(mdns_future, resolvers_future);
        })
        .collect::<FuturesUnordered<_>>()
        .collect::<()>()
        .await;
}

impl Publisher {
    pub fn spawn(listeners: Arc<QuicListeners>, resolvers: HashMap<String, ServerConfig>) -> Self {
        let resolvers = Arc::new(resolvers);

        let publish_all =
            async move |listeners: Arc<QuicListeners>,
                        resolvers: Arc<HashMap<String, ServerConfig>>| {
                publish_once(&listeners, &resolvers).await
            };

        let _task = AbortOnDropHandle::new(tokio::spawn(async move {
            let new_publish_task = || {
                let publish_all = publish_all(listeners.clone(), resolvers.clone());
                AbortOnDropHandle::new(tokio::spawn(async move {
                    // 过滤抖动
                    time::sleep(Duration::from_millis(50)).await;
                    publish_all.await
                }))
            };

            let mut interval = interval(DNS_PUBLISH_INTERVAL);
            interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
            let mut current_publish_task = new_publish_task();
            let mut observer = Locations::global().subscribe();
            loop {
                tokio::select! {
                    _ = observer.recv() => {
                        current_publish_task.abort();
                    }
                    _ = interval.tick() => {
                        current_publish_task.abort();
                    }
                }
                current_publish_task = new_publish_task();
            }
        }));
        Self { _task }
    }
}
