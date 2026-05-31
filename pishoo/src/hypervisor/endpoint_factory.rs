//! Endpoint construction helpers for root-managed listeners.
//!
//! RootState owns resource arbitration. The mechanics of constructing DHTTP
//! resolver stacks live here so the registry code does not duplicate Endpoint
//! builder internals.

use std::sync::Arc;

use dhttp::{
    ddns::resolvers::{DHTTP_H3_DNS_SERVER, DnsScheme, Resolvers},
    dquic::{Network, QuicEndpoint, binds::BindPattern, connection::Connection as QuicConnection},
    endpoint::Endpoint,
    h3x::endpoint::H3Endpoint,
    identity::Identity,
};
use http::Uri;
use snafu::{ResultExt, Snafu};

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum BuildEndpointResolverError {
    #[snafu(display("failed to attach h3 resolver"))]
    H3Resolver { source: std::io::Error },
}

pub async fn build_resolver(
    identity: Arc<Identity>,
    network: Arc<Network>,
    bind_patterns: Arc<Vec<BindPattern>>,
    dns_resolver_url: Option<Uri>,
) -> Result<Resolvers, BuildEndpointResolverError> {
    let h3 = create_h3_dns_endpoint(Some(identity), network.clone(), bind_patterns.clone()).await;
    let base_url = dns_resolver_url
        .map(|url| url.to_string())
        .unwrap_or_else(|| DHTTP_H3_DNS_SERVER.to_owned());

    let builder = Resolvers::builder()
        .mdns(network, bind_patterns)
        .await
        .system()
        .h3_with_base_url(base_url, h3)
        .context(build_endpoint_resolver_error::H3ResolverSnafu)?;

    Ok(builder.build())
}

async fn create_h3_dns_endpoint(
    identity: Option<Arc<Identity>>,
    network: Arc<Network>,
    bind_patterns: Arc<Vec<BindPattern>>,
) -> Arc<H3Endpoint<QuicEndpoint, QuicConnection>> {
    let quic = QuicEndpoint::builder()
        .network(network)
        .maybe_identity(identity)
        .client(dhttp::trust::default_client_quic_config())
        .bind(bind_patterns)
        .build()
        .await;
    Arc::new(H3Endpoint::new(quic))
}

pub async fn build_registered_endpoint(
    identity: Arc<Identity>,
    network: Arc<Network>,
    server_qcfg: dhttp::dquic::server::ServerQuicConfig,
    bind_patterns: Arc<Vec<BindPattern>>,
    resolver: Resolvers,
) -> Result<Endpoint, dhttp::endpoint::InvalidEndpointIdentityError> {
    Endpoint::builder()
        .network(network)
        .identity(identity)
        .server(server_qcfg)
        .bind(bind_patterns)
        .resolver(Arc::new(resolver))
        .build()
        .await
}

pub async fn build_connector_endpoint(
    network: Arc<Network>,
    identity: Option<Identity>,
) -> Result<Endpoint, dhttp::endpoint::InvalidEndpointIdentityError> {
    Endpoint::builder()
        .network(network)
        .maybe_identity(identity.map(Arc::new))
        .dns(DnsScheme::H3)
        .dns(DnsScheme::Mdns)
        .dns(DnsScheme::System)
        .build()
        .await
}
