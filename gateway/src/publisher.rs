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

pub type ServerConfig = (
    Resolvers,
    u8,
    Option<(Arc<dyn SigningKey>, SignatureScheme)>,
);

pub const DNS_PUBLISH_INTERVAL: Duration = Duration::from_secs(10);

// Initialize Mdns for a given device and bind it to the interface
fn ensure_mdns_for_device(bind_iface: &BindInterface, device: &str) -> Option<()> {
    // Check if Mdns already exists for this interface
    let has_component = bind_iface.borrow().with_component(|_: &Mdns| true).is_ok();

    if has_component {
        return Some(());
    }

    // Create socket address for mDNS
    let socket_addr = match SocketAddr::try_from(&BindUri::from(format!(
        "iface://v4.{device}:5353"
    ))) {
        Ok(socket_addr) => socket_addr,
        Err(error) => {
            tracing::debug!(target: "dns", "Failed to resolve IPv4 addr for mDNS on {device}: {error}");
            return None;
        }
    };

    let SocketAddr::V4(socket_addr) = socket_addr else {
        return None;
    };

    // Create Mdns
    let mdns = match Mdns::new(MDNS_SERVICE, *socket_addr.ip(), device) {
        Ok(mdns) => mdns,
        Err(error) => {
            tracing::debug!(target: "dns", "Failed to create mDNS for {device}: {error}");
            return None;
        }
    };

    // Initialize Mdns in the interface's component system
    bind_iface.with_components_mut(|components, _iface| {
        components.init_with(|| mdns);
    });

    Some(())
}

fn endpoint_to_dns_endpoint(
    server_id: u8,
    key: Option<(&dyn SigningKey, SignatureScheme)>,
    endpoint: SocketEndpointAddr,
) -> Option<DnsEndpointAddr> {
    let mut ep: DnsEndpointAddr = endpoint.try_into().ok()?;

    ep.set_main(server_id == 0);
    ep.set_sequence(server_id as u64);

    if let Some((key, scheme)) = key {
        let _ = ep.sign_with(key, scheme);
    }

    Some(ep)
}

async fn get_stun_endpoints(iface: &gm_quic::qinterface::Interface) -> Vec<SocketEndpointAddr> {
    // Get STUN-derived external endpoints if available
    let maybe_join_set: Option<tokio::task::JoinSet<_>> = iface
        .with_component(|clients: &StunClientsComponent| {
            clients.with_clients(|clients| {
                clients
                    .values()
                    .cloned()
                    .map(|client| async move {
                        let agent = client.agent_addr();
                        let outer = client.outer_addr().await?;
                        std::io::Result::Ok(SocketEndpointAddr::with_agent(agent, outer))
                    })
                    .collect::<tokio::task::JoinSet<_>>()
            })
        })
        .ok()
        .flatten();

    if let Some(mut join_set) = maybe_join_set {
        let mut out = vec![];
        while let Some(joined) = join_set.join_next().await {
            if let Ok(Ok(ep)) = joined {
                out.push(ep);
            }
        }
        return out;
    }

    // No STUN endpoints available
    vec![]
}

async fn do_publish_server(
    server_name: &str,
    server_id: u8,
    resolver: &Resolver,
    _key: Option<(&dyn SigningKey, SignatureScheme)>,
    endpoint_addresses: &[DnsEndpointAddr],
) {
    tracing::debug!(target: "dns", server_name, server_id, "Publishing dns for server");

    if let Err(error) = resolver.publish(server_name, endpoint_addresses).await {
        tracing::error!(
            target: "dns",
            "Resolver `{resolver}` publish dns failed: {}",
            Report::from_error(error),
        );
    }
}

#[tracing::instrument(level = "info", skip_all, fields(server_name, server_id))]
async fn publish_mdns(
    server_name: &str,
    server_id: u8,
    key: Option<(Arc<dyn SigningKey>, SignatureScheme)>,
    interfaces: impl IntoIterator<Item = (&BindUri, &BindInterface)>,
) {
    let mut publish_futures: Vec<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>> =
        Vec::new();

    for (bind_uri, bind_iface) in interfaces {
        let Some((_ip_family, device, _port)) = bind_uri.as_iface_bind_uri() else {
            continue;
        };

        // Ensure Mdns exists for this device
        ensure_mdns_for_device(bind_iface, device);

        let iface = bind_iface.borrow();

        // Get local address for mDNS publishing
        let local_addr = match iface.real_addr().ok() {
            Some(RealAddr::Internet(addr)) => addr,
            _ => {
                tracing::debug!(target: "dns", %bind_uri, device, "No local internet address available for mDNS publish, skipping");
                continue;
            }
        };

        // Create MdnsResolver for publishing
        let ip = match local_addr {
            SocketAddr::V4(addr) => *addr.ip(),
            _ => continue,
        };
        let resolver = match MdnsResolver::new(MDNS_SERVICE, ip, device) {
            Ok(resolver) => Arc::new(resolver),
            Err(error) => {
                tracing::debug!(target: "dns", "Failed to create mDNS resolver for {device}: {error}");
                continue;
            }
        };

        let key = key.clone();
        let server_name = server_name.to_string();
        let bind_uri = bind_uri.clone();

        let future = async move {
            let mut dns_ep = match local_addr {
                SocketAddr::V4(addr) => DnsEndpointAddr::direct_v4(addr),
                SocketAddr::V6(addr) => DnsEndpointAddr::direct_v6(addr),
            };
            dns_ep.set_main(server_id == 0);
            dns_ep.set_sequence(server_id as u64);
            if let Some((key, scheme)) = key.as_ref() {
                let _ = dns_ep.sign_with(key.as_ref(), *scheme);
            }

            do_publish_server(
                &server_name,
                server_id,
                resolver.as_ref(),
                key.as_ref().map(|(k, s)| (k.as_ref(), *s)),
                &[dns_ep],
            )
            .await;
        };

        tracing::debug!(target: "dns", %bind_uri, device, %local_addr, "Publishing local address to mDNS");
        publish_futures.push(Box::pin(future));
    }

    // Execute all publish futures concurrently
    futures::future::join_all(publish_futures).await;
}

#[tracing::instrument(level = "info", skip_all, fields(server_name, server_id))]
async fn publish_resolvers(
    server_name: &str,
    server_id: u8,
    resolvers: &Resolvers,
    key: Option<(Arc<dyn SigningKey>, SignatureScheme)>,
    interfaces: impl IntoIterator<Item = (&BindUri, &BindInterface)>,
) {
    let mut endpoint_addresses = vec![];

    for (_bind_uri, bind_iface) in interfaces {
        let iface = bind_iface.borrow();

        // Get STUN external endpoints for DNS publishing
        for endpoint in get_stun_endpoints(&iface).await {
            if let Some(dns_ep) = endpoint_to_dns_endpoint(
                server_id,
                key.as_ref().map(|(k, s)| (k.as_ref(), *s)),
                endpoint,
            ) {
                endpoint_addresses.push(dns_ep);
            }
        }
    }

    if endpoint_addresses.is_empty() {
        tracing::debug!(target: "dns", server_name, server_id, "No STUN endpoints available for DNS publish, skipping");
        return;
    }

    tracing::debug!(target: "dns", server_name, server_id, endpoint_count = endpoint_addresses.len(), "Publishing STUN endpoints to DNS");

    for resolver in resolvers {
        do_publish_server(
            server_name,
            server_id,
            resolver.as_ref(),
            key.as_ref().map(|(k, s)| (k.as_ref(), *s)),
            &endpoint_addresses,
        )
        .await;
    }
}
async fn publish_once(listeners: &Arc<QuicListeners>, resolvers: &HashMap<String, ServerConfig>) {
    let servers = listeners
        .servers()
        .into_iter()
        .filter_map(|server_name| {
            let interfaces = listeners.get_server(&server_name)?.bind_interfaces();
            let (resolvers, id, key) = resolvers.get(&server_name)?;
            Some((server_name, (interfaces, resolvers, *id, key.clone())))
        })
        .collect::<HashMap<_, _>>();

    servers
        .iter()
        .map(
            |(server_name, (interfaces, resolvers, id, key))| async move {
                publish_resolvers(server_name, *id, resolvers, key.clone(), interfaces.iter())
                    .await;
                publish_mdns(server_name, *id, key.clone(), interfaces.iter()).await;
            },
        )
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
