use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

use futures::StreamExt;
use gm_quic::QuicListeners;
use qconnection::prelude::SocketEndpointAddr;
use qdns::Resolve;
use qinterface::{QuicIoExt, iface::monitor::InterfacesMonitor};
use snafu::Report;
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::task::AbortOnDropHandle;

pub struct Publisher {
    _task: AbortOnDropHandle<()>,
}

impl Publisher {
    pub fn spawn(
        listeners: Arc<QuicListeners>,
        resolvers: HashMap<String, Vec<Arc<dyn Resolve + Send + Sync>>>,
    ) -> Self {
        let _task = AbortOnDropHandle::new(tokio::spawn(async move {
            let mut network = InterfacesMonitor::global().subscribe();
            let mut interval = interval(Duration::from_secs(20));
            interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = network.changed() => {}
                    _ = interval.tick() => {}
                }
                for (server, resolvers) in &resolvers {
                    tracing::debug!(target: "dns", "Try to publish dns for {server}");
                    let Some(interfaces) = listeners
                        .get_server(server)
                        .map(|server| server.bind_interfaces())
                    else {
                        // 内部错误，不展示
                        tracing::debug!(target: "dns", "Publish dns for {server} failed: No such server in listeners");
                        continue;
                    };

                    let local_endpoint_addrs = interfaces
                        .keys()
                        .filter_map(|bind_uri| SocketAddr::try_from(bind_uri).ok())
                        .map(SocketEndpointAddr::direct)
                        .collect::<Vec<_>>();

                    let endpoint_addrs = futures::stream::iter(interfaces.values())
                        .filter_map(|iface| async move { iface.borrow().ok() })
                        .filter_map(|iface| async move { iface.endpoint_addr().await.ok() })
                        .collect::<Vec<_>>()
                        .await;

                    for resolver in resolvers {
                        let addresses = if resolver.server().contains(".local") {
                            &local_endpoint_addrs
                        } else {
                            &endpoint_addrs
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
                    }
                }
            }
        }));
        Self { _task }
    }
}
