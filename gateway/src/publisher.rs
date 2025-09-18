use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

use futures::StreamExt;
use gm_quic::QuicListeners;
use qconnection::prelude::SocketEndpointAddr;
use qdns::Resolve;
use qinterface::{QuicIoExt, iface::monitor::InterfacesMonitor};
use snafu::Report;
use tokio::{
    sync::OnceCell,
    time::{MissedTickBehavior, interval},
};
use tokio_util::task::AbortOnDropHandle;

pub struct Publisher {
    _task: AbortOnDropHandle<()>,
}

pub type ServerResolvers = Vec<Arc<dyn Resolve + Send + Sync>>;

pub const DNS_PUBLISH_INTERVAL: Duration = Duration::from_secs(10);

#[tracing::instrument(
    level = "debug", skip_all, 
    fields(%server, resolvers = ?resolvers.iter().map(|resolver| resolver.server()).collect::<Vec<_>>())
)]
pub async fn publish_server(
    listeners: &Arc<QuicListeners>,
    server: &str,
    resolvers: &ServerResolvers,
) {
    tracing::debug!(target: "dns", "Try to publish dns for {server}");
    let Some(interfaces) = listeners
        .get_server(server)
        .map(|server| server.bind_interfaces())
    else {
        // 内部错误，不展示
        tracing::debug!(target: "dns", "Publish dns for {server} failed: No such server in listeners");
        return;
    };

    // lazy eval
    let local_endpoint_addrs = OnceCell::new();
    let local_endpoint_addrs = async || {
        local_endpoint_addrs
            .get_or_init(async || {
                interfaces
                    .keys()
                    .filter_map(|bind_uri| SocketAddr::try_from(bind_uri).ok())
                    .map(SocketEndpointAddr::direct)
                    .collect::<Vec<_>>()
            })
            .await
    };

    // lazy eval
    let endpoint_addrs = OnceCell::new();
    let endpoint_addrs = async || {
        endpoint_addrs
            .get_or_init(async || {
                futures::stream::iter(interfaces.values())
                    .filter_map(|iface| async move { iface.borrow().ok() })
                    .filter_map(|iface| async move { iface.endpoint_addr().await.ok() })
                    .collect::<Vec<_>>()
                    .await
            })
            .await
    };

    let publish_server_dns = resolvers.iter().map(async |resolver| {
        let addresses = if resolver.server().contains(".local") {
            local_endpoint_addrs().await
        } else {
            endpoint_addrs().await
        };
        match resolver.publish(server, addresses).await {
            Ok(..) => tracing::debug!(
                target: "dns",
                "Publish dns {addresses:?} for {server} to {} success",
                resolver.server()
            ),
            Err(error) => {
                let addresses = addresses
                    .iter()
                    .map(|addr| addr.to_string())
                    .collect::<Vec<_>>();
                tracing::error!(
                    target: "dns",
                    "Publish dns {addresses:?} for {server} to {} failed: {}",
                    resolver.server(), Report::from_error(error),
                );
            }
        }
    });
    futures::future::join_all(publish_server_dns).await;
}

impl Publisher {
    pub fn spawn(
        listeners: Arc<QuicListeners>,
        resolvers: HashMap<String, ServerResolvers>,
    ) -> Self {
        let resolvers = Arc::new(resolvers);

        let publish_all =
            async move |listeners: Arc<QuicListeners>,
                        resolvers: Arc<HashMap<String, ServerResolvers>>| {
                futures::future::join_all(resolvers.iter().map(async |(server, resolvers)| {
                    publish_server(&listeners, server, resolvers).await
                }))
                .await;
            };

        let _task = AbortOnDropHandle::new(tokio::spawn(async move {
            let new_publish_task = || {
                AbortOnDropHandle::new(tokio::spawn(publish_all(
                    listeners.clone(),
                    resolvers.clone(),
                )))
            };
            let mut network = InterfacesMonitor::global().subscribe();
            let mut interval = interval(DNS_PUBLISH_INTERVAL);
            interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
            let mut current_publish_task = new_publish_task();
            loop {
                tokio::select! {
                    _ = network.changed() => {}
                    _ = interval.tick() => {}
                }
                current_publish_task.abort();
                current_publish_task = new_publish_task();
            }
        }));
        Self { _task }
    }
}
