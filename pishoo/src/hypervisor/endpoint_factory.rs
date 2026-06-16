//! Endpoint construction helpers for root-managed listeners.
//!
//! RootState owns resource arbitration. The mechanics of constructing DHTTP
//! resolver stacks live here so the registry code does not duplicate Endpoint
//! builder internals.

use std::sync::Arc;

use dhttp::{
    ddns::resolvers::{DnsScheme, Resolvers},
    dquic::{Network, binds::BindPattern},
    endpoint::Endpoint,
    identity::Identity,
};
use http::Uri;
use snafu::{ResultExt, Snafu};

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum BuildEndpointResolverError {
    #[snafu(display("failed to build h3 resolver endpoint"))]
    BuildEndpoint {
        source: dhttp::endpoint::InvalidEndpointIdentityError,
    },
    #[snafu(display("failed to attach h3 resolver"))]
    H3Resolver { source: std::io::Error },
}

pub async fn build_resolver(
    identity: Arc<Identity>,
    network: Arc<Network>,
    bind_patterns: Arc<Vec<BindPattern>>,
    dns_resolver_url: Option<Uri>,
) -> Result<Resolvers, BuildEndpointResolverError> {
    let h3 = create_h3_dns_endpoint(Some(identity), network.clone(), bind_patterns.clone())
        .await
        .context(build_endpoint_resolver_error::BuildEndpointSnafu)?;

    let builder = Resolvers::builder()
        .mdns(network, bind_patterns)
        .await
        .system();
    let builder = match dns_resolver_url {
        Some(url) => builder.h3_with_base_url(url.to_string(), h3),
        None => builder.h3(h3),
    }
    .context(build_endpoint_resolver_error::H3ResolverSnafu)?;

    Ok(builder.build())
}

async fn create_h3_dns_endpoint(
    identity: Option<Arc<Identity>>,
    network: Arc<Network>,
    bind_patterns: Arc<Vec<BindPattern>>,
) -> Result<Arc<dhttp::h3x::dquic::H3Endpoint>, dhttp::endpoint::InvalidEndpointIdentityError> {
    let endpoint = Endpoint::builder()
        .network(network.into())
        .maybe_identity(identity)
        .bind(bind_patterns)
        .dns(DnsScheme::System)
        .build()
        .await;
    endpoint.map(|endpoint| endpoint.as_h3())
}

pub async fn build_registered_endpoint_with_resolver(
    identity: Arc<Identity>,
    network: Arc<Network>,
    bind_patterns: Arc<Vec<BindPattern>>,
    resolver: Resolvers,
) -> Result<Endpoint, dhttp::endpoint::InvalidEndpointIdentityError> {
    Endpoint::builder()
        .network(network.into())
        .identity(identity)
        .bind(bind_patterns)
        .resolver(Arc::new(resolver))
        .build()
        .await
}

pub async fn build_registered_endpoint(
    identity: Arc<Identity>,
    network: Arc<Network>,
    bind_patterns: Arc<Vec<BindPattern>>,
) -> Result<Endpoint, dhttp::endpoint::InvalidEndpointIdentityError> {
    Endpoint::builder()
        .network(network.into())
        .identity(identity)
        .bind(bind_patterns)
        .dns(DnsScheme::H3)
        .dns(DnsScheme::Mdns)
        .dns(DnsScheme::System)
        .build()
        .await
}

pub async fn build_connector_endpoint(
    network: Arc<Network>,
    identity: Option<Identity>,
) -> Result<Endpoint, dhttp::endpoint::InvalidEndpointIdentityError> {
    Endpoint::builder()
        .network(network.into())
        .maybe_identity(identity.map(Arc::new))
        .dns(DnsScheme::H3)
        .dns(DnsScheme::Mdns)
        .dns(DnsScheme::System)
        .build()
        .await
}
