use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

use futures::{StreamExt, stream::FuturesUnordered};
use gm_quic::{
    prelude::{BindUri, QuicListeners, RealAddr},
    qbase::net::route::SocketEndpointAddr,
    qinterface::{BindInterface, component::location::Locations, io::IO},
    qtraversal::nat::client::StunClientsComponent,
};
use gmdns::{
    mdns::Mdns,
    parser::record::endpoint::EndpointAddr as DnsEndpointAddr,
    resolver::{MDNS_SERVICE, MdnsResolver, Publisher as DnsPublisher},
};
use rustls::{SignatureScheme, sign::SigningKey};
use snafu::Report;
use tokio::time::{self, MissedTickBehavior, interval};
use tokio_util::task::AbortOnDropHandle;

pub struct Publisher {
    _task: AbortOnDropHandle<()>,
}

pub type Resolver = dyn DnsPublisher + Send + Sync;
pub type Resolvers = Vec<Arc<Resolver>>;

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

pub const DNS_PUBLISH_INTERVAL: Duration = Duration::from_secs(10);
const MAIN_SERVER_ID: u8 = 0;

// Initialize Mdns for a given device if not present
fn ensure_mdns_for_device(bind_iface: &BindInterface, device: &str) {
    if matches!(
        bind_iface.borrow().with_component(|_: &Mdns| ()),
        Ok(Some(_))
    ) {
        return;
    }

    if let Ok(SocketAddr::V4(addr)) =
        SocketAddr::try_from(&BindUri::from(format!("iface://v4.{device}:5353")))
        && let Ok(mdns) = Mdns::new(MDNS_SERVICE, *addr.ip(), device)
    {
        bind_iface.with_components_mut(|components, _| {
            components.init_with(move || mdns);
        });
    }
}

fn get_stun_endpoints(iface: &gm_quic::qinterface::Interface) -> Vec<SocketEndpointAddr> {
    iface
        .with_component(|clients: &StunClientsComponent| {
            clients.with_clients(|clients| {
                clients
                    .values()
                    .filter_map(|client| {
                        let outer = client.get_outer_addr()?.ok()?;
                        Some(SocketEndpointAddr::with_agent(client.agent_addr(), outer))
                    })
                    .collect()
            })
        })
        .ok()
        .flatten()
        .unwrap_or_default()
}

async fn publish_single_mdns(
    bind_uri: &BindUri,
    bind_iface: &BindInterface,
    server_name: &str,
    config: &ServerConfig,
) {
    let Some((_, device, _)) = bind_uri.as_iface_bind_uri() else {
        return;
    };

    ensure_mdns_for_device(bind_iface, device);

    let iface = bind_iface.borrow();
    let Ok(RealAddr::Internet(local_addr)) = iface.real_addr() else {
        return;
    };

    let SocketAddr::V4(ip) = local_addr else {
        return;
    };

    let Ok(resolver) = MdnsResolver::new(MDNS_SERVICE, *ip.ip(), device) else {
        return;
    };

    let mut ep = DnsEndpointAddr::direct_v4(ip);
    config.sign_endpoint(&mut ep);

    if let Err(error) = resolver.publish(server_name, &[ep]).await {
        tracing::error!(
            target: "dns",
            "Resolve `{resolver}` publish dns failed: {}",
            Report::from_error(error)
        );
    } else {
        tracing::debug!(target: "dns", %bind_uri, device, %local_addr, "Publishing local address to mDNS");
    }
}

async fn publish_resolvers(
    server_name: &str,
    config: &ServerConfig,
    interfaces: impl Iterator<Item = (&BindUri, &BindInterface)>,
) {
    let mut endpoints = vec![];
    for (_, bind_iface) in interfaces {
        for sock_ep in get_stun_endpoints(&bind_iface.borrow()) {
            if let Ok(mut ep) = DnsEndpointAddr::try_from(sock_ep) {
                config.sign_endpoint(&mut ep);
                endpoints.push(ep);
            }
        }
    }

    if endpoints.is_empty() {
        return;
    }

    tracing::debug!(target: "dns", server_name, server_id = config.server_id, count = endpoints.len(), "Publishing STUN endpoints");

    for resolver in &config.resolvers {
        if let Err(error) = resolver.publish(server_name, &endpoints).await {
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
            publish_resolvers(&name, config, ifaces.iter()).await;

            ifaces
                .iter()
                .map(|(uri, iface)| publish_single_mdns(uri, iface, &name, config))
                .collect::<FuturesUnordered<_>>()
                .collect::<()>()
                .await;
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
