use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};

use dashmap::DashMap;
use futures::{StreamExt, stream::FuturesUnordered};
use gm_quic::{QuicIO, QuicListeners, RealAddr};
use qconnection::prelude::{BindUri, SocketEndpointAddr};
use qdns::{MDNS_SERVICE, MdnsResolver, Resolve};
use qinterface::{
    QuicIoExt,
    iface::{QuicInterface, physical::PhysicalInterfaces},
    local::Locations,
};
use snafu::{Report, ResultExt};
use tokio::time::{self, MissedTickBehavior, interval};
use tokio_util::task::AbortOnDropHandle;

use crate::error::Whatever;

pub struct Publisher {
    _task: AbortOnDropHandle<()>,
}

pub type Resolver = dyn Resolve + Send + Sync;
pub type Resolvers = Vec<Arc<Resolver>>;
pub type MDnsResolvers = DashMap<String, Arc<MdnsResolver>>;

pub const DNS_PUBLISH_INTERVAL: Duration = Duration::from_secs(10);

async fn initial_mdns_resolvers<'b>(
    bind_uris: impl IntoIterator<Item = &'b BindUri>,
) -> impl Iterator<Item = (String, Arc<MdnsResolver>)> + 'b {
    let physical_interfaces = PhysicalInterfaces::global().interfaces();
    bind_uris
        .into_iter()
        .filter_map(|bind_uri| {
            bind_uri
                .as_iface_bind_uri()
                .map(|(_ip_family, device, _port)| device)
        })
        .collect::<HashSet<_>>()
        .into_iter()
        .filter_map(move |device| {
            let mdns_resolver = (|| {
                let socket_addr = BindUri::from(format!("iface://v4.{device}:5353"))
                    .resolve(physical_interfaces.get(device))
                    .whatever_context(format!("Failed to create mDNS resolver for {device}"))?;
                let SocketAddr::V4(socket_addr) = socket_addr else {
                    unreachable!()
                };

                let mdns_resolver = MdnsResolver::new(MDNS_SERVICE, *socket_addr.ip(), device)
                    .whatever_context(format!("Failed to create mDNS resolver for {device}"))?;
                Result::<_, Whatever>::Ok(mdns_resolver)
            })();

            match mdns_resolver {
                Ok(resolver) => Some((device.to_owned(), Arc::new(resolver))),
                Err(error) => {
                    tracing::debug!(
                        target: "dns",
                        "Some addresses will not be publish: {}",
                        Report::from_error(error),
                    );
                    None
                }
            }
        })
}

async fn do_publish_server(
    server: &str,
    resolver: &Resolver,
    endpoint_addresses: &[SocketEndpointAddr],
) {
    match resolver.publish(server, endpoint_addresses).await {
        Ok(..) => tracing::debug!(
            target: "dns",
            "Resolver `{resolver}` publish dns {endpoint_addresses:?} success",
        ),
        Err(error) => {
            let addresses = endpoint_addresses
                .iter()
                .map(|addr| addr.to_string())
                .collect::<Vec<_>>();
            tracing::error!(
                target: "dns",
                "Resolver `{resolver}` publish dns {addresses:?} failed: {}",
                Report::from_error(error),
            );
        }
    }
}

#[tracing::instrument(level = "info", skip_all, fields(%server))]
async fn publish_dns(
    server: &str,
    resolvers: &Resolvers,
    interfaces: impl IntoIterator<Item = (&BindUri, &QuicInterface)>,
) {
    let endpoint_addresses = futures::stream::iter(interfaces.into_iter())
        .filter_map(async |(bind_uri, interface)| {
            let endpoint_addr = match interface.endpoint_addr().await {
                Ok(addr) => {
                    tracing::debug!(target: "dns", bind_uri=%bind_uri, %addr, "Get endpoint addr for publish");
                    Ok(addr)
                }
                Err(error) => {
                    tracing::debug!(target: "dns", %bind_uri, "Get endpoint addr error, skip dns publish: {}", Report::from_error(&error));
                    Err(error)
                }
            };
            endpoint_addr.ok()
        })
        .collect::<Vec<_>>()
        .await;
    match endpoint_addresses.as_slice() {
        [] => tracing::warn!(target: "dns", "No endpoint addresses to publish, skip"),
        _ => {
            resolvers
                .iter()
                .map(|resolver| do_publish_server(server, resolver.as_ref(), &endpoint_addresses))
                .collect::<FuturesUnordered<_>>()
                .collect::<()>()
                .await;
        }
    }
}

#[tracing::instrument(level = "info", skip_all, fields(%server))]
async fn publish_mdns(
    server: &str,
    resolvers: &MDnsResolvers,
    interfaces: impl IntoIterator<Item = (&BindUri, &QuicInterface)>,
) {
    interfaces
        .into_iter()
        .fold(
            HashMap::<String, (Arc<MdnsResolver>, Vec<SocketEndpointAddr>)>::new(),
            |mut map, (bind_uri, interface)| {
                if let Some((_ip_family, device, _port)) = bind_uri.as_iface_bind_uri() {
                    let socket_addr = match interface.real_addr() {
                        Ok(RealAddr::Internet(socket_addr)) => socket_addr,
                        Ok(_) => {
                            tracing::debug!(target: "dns", %bind_uri, "Unsupported address kind, skip");
                            return map;
                        }
                        Err(error) => {
                            tracing::debug!(target: "dns", %bind_uri, "Get real addr error, skip: {}", Report::from_error(&error));
                            return map;
                        }
                    };
                    let Some(resolver) = resolvers.get(device) else {
                        tracing::warn!(target: "dns", %bind_uri, "No mDNS resolver for the device, skip");
                        return map;
                    };
                    map.entry(device.to_owned())
                        .or_insert_with(|| (resolver.clone(), vec![]))
                        .1
                        .push(SocketEndpointAddr::from(socket_addr));
                };

                map
            },
        )
        .into_iter()
        .map(|(_device, (resolver, addresses))| async move {
            do_publish_server(server, resolver.as_ref(), &addresses).await
        })
        .collect::<FuturesUnordered<_>>()
        .collect::<()>()
        .await;
}

#[tracing::instrument(level = "debug", skip_all)]
async fn publish_once(
    listeners: &Arc<QuicListeners>,
    resolvers: &HashMap<String, Resolvers>,
    mdns_resolvers: &MDnsResolvers,
) {
    let servers = (listeners.servers().into_iter())
        .filter_map(|server| {
            let interfaces = listeners.get_server(&server)?.bind_interfaces();
            let interfaces = interfaces.into_iter().filter_map(|(bind_uri, interface)| {
                match interface.borrow() {
                    Ok(interface) => Some((bind_uri, interface)),
                    Err(error) => {
                        tracing::debug!(target: "dns", %bind_uri, "Get interface error, skip: {}", Report::from_error(&error));
                        None
                    }
                }
            }).collect::<HashMap<_, _>>();
            let resolvers = resolvers.get(&server)?;
            Some((server, (interfaces, resolvers)))
        })
        .collect::<HashMap<_, _>>();

    let publish_dns = servers
        .iter()
        .map(|(server, (ifaces, resolvers))| publish_dns(server, resolvers, ifaces.iter()))
        .collect::<FuturesUnordered<_>>()
        .collect::<()>();

    let publish_mdns = async {
        mdns_resolvers.clear();
        for (device, mdns) in
            initial_mdns_resolvers(servers.values().flat_map(|(ifaces, _)| ifaces.keys())).await
        {
            mdns_resolvers.insert(device, mdns);
        }
        servers
            .iter()
            .map(|(server, (ifaces, _))| publish_mdns(server, mdns_resolvers, ifaces.iter()))
            .collect::<FuturesUnordered<_>>()
            .collect::<()>()
            .await;
    };

    futures::join!(publish_dns, publish_mdns);
}

impl Publisher {
    pub fn spawn(listeners: Arc<QuicListeners>, resolvers: HashMap<String, Resolvers>) -> Self {
        let resolvers = Arc::new(resolvers);
        let mdns_resolvers = Arc::new(MDnsResolvers::new());

        let publish_all =
            async move |listeners: Arc<QuicListeners>,
                        resolvers: Arc<HashMap<String, Resolvers>>,
                        mdns_resolvers: Arc<DashMap<String, Arc<MdnsResolver>>>| {
                publish_once(&listeners, &resolvers, &mdns_resolvers).await
            };

        let _task = AbortOnDropHandle::new(tokio::spawn(async move {
            let new_publish_task = || {
                let publish_all =
                    publish_all(listeners.clone(), resolvers.clone(), mdns_resolvers.clone());
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
