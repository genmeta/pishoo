use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

use futures::{StreamExt, stream::FuturesUnordered};
use gm_quic::{
    prelude::{BindUri, BoundAddr, IO, QuicListeners},
    qbase::net::addr::SocketEndpointAddr,
    qdns::Publish as DnsPublisher,
    qinterface::{BindInterface, component::location::Locations},
    qtraversal::nat::client::{NatType, StunClientsComponent},
};
use gmdns::{
    MdnsPacket,
    mdns::Mdns,
    parser::record::endpoint::EndpointAddr as DnsEndpointAddr,
    resolvers::{H3Publisher, MdnsResolver},
};
use rustls::{SignatureScheme, sign::SigningKey};
use snafu::{OptionExt, Report, ResultExt, whatever};
use tokio::time::{self, MissedTickBehavior, interval};
use tokio_util::task::AbortOnDropHandle;
use tracing::{Instrument, info};

use crate::{
    dns::{MDNS_SERVICE, resolve::DnsResolver},
    error::{Result, Whatever},
    parse::{Node, ServerIdentity, Value, server_identity},
};

pub struct Publisher {
    _task: AbortOnDropHandle<()>,
}

pub type Resolvers = Vec<Arc<dyn DnsPublisher + Send + Sync>>;

#[derive(Clone)]
pub struct PublishConfig {
    pub resolvers: Resolvers,
    pub server_id: u8,
    pub signing_key: Option<(Arc<dyn SigningKey>, SignatureScheme)>,
}

impl PublishConfig {
    pub(crate) fn sign_endpoint(&self, ep: &mut DnsEndpointAddr) {
        ep.set_main(self.server_id == MAIN_SERVER_ID);
        ep.set_sequence(self.server_id as u64);
        if let Some((key, scheme)) = &self.signing_key
            && let Err(e) = ep.sign_with(key.as_ref(), *scheme)
        {
            tracing::warn!(error = %Report::from_error(e), "failed to sign endpoint");
        }
    }
}

/// 将一组 endpoint 签名后发布到指定主机名。
///
/// 该函数只负责 DNS 发布；endpoint 的筛选与来源由调用方决定。
pub async fn publish_host_endpoints(
    host: &str,
    mut endpoints: Vec<DnsEndpointAddr>,
    config: &PublishConfig,
) {
    if config.resolvers.is_empty() {
        tracing::warn!(
            host,
            "no dns publisher resolver available, cannot publish endpoints"
        );
        return;
    }

    if endpoints.is_empty() {
        tracing::warn!(host, "no endpoints to publish");
        return;
    }

    endpoints.iter_mut().for_each(|ep| config.sign_endpoint(ep));

    let mut hosts = HashMap::new();
    hosts.insert(host.to_string(), endpoints);
    let packet = MdnsPacket::answer(0, &hosts).to_bytes();

    for resolver in &config.resolvers {
        if let Err(error) = resolver.publish(host, &packet).await {
            tracing::error!(
                host,
                resolver = %resolver,
                error = %Report::from_error(error),
                "dns publish failed"
            );
        } else {
            tracing::info!(host, "published endpoints");
        }
    }
}

fn build_publisher(
    resolver: &DnsResolver,
    config: &ServerIdentity,
) -> Arc<dyn DnsPublisher + Send + Sync> {
    info!(
        server_name = %config.server_name,
        base_url = %resolver.base_url,
        "creating h3 dns publisher"
    );
    Arc::new(
        H3Publisher::new(
            resolver.base_url.to_string(),
            resolver.create_h3_client(config),
        )
        .expect("h3 dns server base_url has been checked"),
    )
}

pub fn build_publish_configs(servers: &[Arc<Node>]) -> Result<HashMap<String, PublishConfig>> {
    let mut configs = HashMap::new();

    for server in servers {
        let key_path = match server.get("ssl_certificate_key") {
            Some(Value::Path(path)) => path,
            _ => whatever!("missing or invalid ssl_certificate_key for server"),
        };

        let resolver = DnsResolver::from_node_or_default(server);
        let signing_key = load_signing_key(key_path).ok();

        let server_names = match server.get("server_name") {
            Some(Value::ServerName(names)) => names,
            _ => unreachable!("invalid server name"),
        };

        for server_name in server_names {
            let domain = match server_name.name.strip_suffix('~') {
                Some(prefix) => format!("{prefix}.genmeta.net"),
                None => server_name.name.clone(),
            };

            let identity = server_identity(server, domain.clone())
                .expect("missing ssl_certificate or ssl_certificate_key");

            let resolvers = if domain.ends_with("user.genmeta.net") {
                tracing::warn!(server_name = %domain, "domain excluded from publishing");
                vec![]
            } else {
                tracing::info!(server_name = %domain, server_id = identity.server_id, "configuring dns publisher");
                vec![build_publisher(&resolver, &identity)]
            };

            configs.insert(
                domain,
                PublishConfig {
                    resolvers,
                    server_id: identity.server_id,
                    signing_key: signing_key.clone(),
                },
            );
        }
    }

    Ok(configs)
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
        Err(error) => {
            tracing::warn!(
                %bind_uri,
                %local_addr,
                device,
                error = %Report::from_error(error),
                "failed to initialize mdns resolver"
            );
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
    config: &PublishConfig,
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
                resolver = %resolver,
                error = %Report::from_error(error),
                "dns publish failed"
            );
        } else {
            tracing::trace!(%bind_uri, %addr, "publishing local address to mdns");
        }
    }
}

async fn publish_resolvers(
    server_name: &str,
    config: &PublishConfig,
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
                            match client.get_nat_type() {
                                Some(Ok(NatType::FullCone)) => {
                                    tracing::debug!(outer = ?outer, "client behind full cone nat, suitable for dns publication");
                                    Some(SocketEndpointAddr::direct(outer))
                                }
                                Some(Ok(_)) => {
                                    tracing::debug!(outer = ?outer, "found stun client with non-full-cone nat for dns publication");
                                    Some(SocketEndpointAddr::with_agent(client.agent_addr(), outer))
                                }
                                _ => {
                                    tracing::debug!(outer = ?outer, "found stun client with unknown nat type for dns publication");
                                    Some(SocketEndpointAddr::with_agent(client.agent_addr(), outer))
                                }
                            }
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
        tracing::warn!(server_name, "no endpoints to publish for this server");
        return;
    }

    tracing::debug!(
        server_name,
        server_id = config.server_id,
        count = endpoints.len(),
        "publishing endpoints"
    );

    let mut hosts = std::collections::HashMap::new();
    hosts.insert(server_name.to_string(), endpoints);
    let packet = MdnsPacket::answer(0, &hosts).to_bytes();

    for resolver in &config.resolvers {
        if let Err(error) = resolver.publish(server_name, &packet).await {
            tracing::error!(
                resolver = %resolver,
                error = %Report::from_error(error),
                "dns publish failed"
            );
        }
    }
}

async fn publish_once(listeners: &Arc<QuicListeners>, resolvers: &HashMap<String, PublishConfig>) {
    listeners
        .servers()
        .into_iter()
        .filter_map(|name| {
            let ifaces = listeners.get_server(&name)?.bind_interfaces();
            let config = resolvers.get(&name)?;
            Some((name, ifaces, config))
        })
        .map(|(name, ifaces, config)| async move {
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
    pub fn spawn(listeners: Arc<QuicListeners>, resolvers: HashMap<String, PublishConfig>) -> Self {
        let resolvers = Arc::new(resolvers);

        let publish_all =
            async move |listeners: Arc<QuicListeners>,
                        resolvers: Arc<HashMap<String, PublishConfig>>| {
                publish_once(&listeners, &resolvers).await
            };

        let _task = AbortOnDropHandle::new(tokio::spawn(
            async move {
                let new_publish_task = || {
                    let publish_all = publish_all(listeners.clone(), resolvers.clone());
                    AbortOnDropHandle::new(tokio::spawn(
                        async move {
                            time::sleep(Duration::from_millis(50)).await;
                            publish_all.await
                        }
                        .in_current_span(),
                    ))
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
            }
            .in_current_span(),
        ));
        Self { _task }
    }
}

fn load_signing_key(path: &std::path::Path) -> Result<(Arc<dyn SigningKey>, SignatureScheme)> {
    use gm_quic::prelude::handy::ToPrivateKey;

    let key_bytes = std::fs::read(path)
        .whatever_context::<_, Whatever>(format!("failed to read key file {}", path.display()))?;
    let key_der = key_bytes.to_private_key();
    let key = rustls::crypto::ring::sign::any_supported_type(&key_der)
        .whatever_context::<_, Whatever>("unsupported key type")?;

    let supported_schemes = [
        SignatureScheme::ECDSA_NISTP256_SHA256,
        SignatureScheme::ECDSA_NISTP384_SHA384,
        SignatureScheme::ED25519,
        SignatureScheme::RSA_PSS_SHA256,
        SignatureScheme::RSA_PSS_SHA384,
        SignatureScheme::RSA_PSS_SHA512,
        SignatureScheme::RSA_PKCS1_SHA256,
        SignatureScheme::RSA_PKCS1_SHA384,
        SignatureScheme::RSA_PKCS1_SHA512,
    ];

    let scheme = supported_schemes
        .iter()
        .find(|&&scheme| key.choose_scheme(&[scheme]).is_some())
        .copied()
        .whatever_context::<_, Whatever>("no supported signature scheme found for key")?;

    Ok((key, scheme))
}
