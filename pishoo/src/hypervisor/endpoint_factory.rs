//! Endpoint construction helpers for root-managed listeners.
//!
//! RootState owns resource arbitration. The mechanics of constructing DHTTP
//! resolver stacks live here so the registry code does not duplicate Endpoint
//! builder internals.

use std::sync::Arc;

use dhttp::{
    ddns::resolvers::DnsScheme,
    dquic::binds::BindPattern,
    endpoint::{BuildEndpointError, Endpoint},
    identity::Identity,
    network::DhttpNetwork,
};
use http::Uri;
use snafu::{ResultExt, Snafu};

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum BuildRegisteredEndpointError {
    #[snafu(display("failed to build endpoint"))]
    Endpoint { source: BuildEndpointError },
}

pub async fn build_registered_endpoint(
    identity: Arc<Identity>,
    network: DhttpNetwork,
    bind_patterns: Arc<Vec<BindPattern>>,
    h3_dns_server: Option<Uri>,
) -> Result<Endpoint, BuildRegisteredEndpointError> {
    let builder = Endpoint::builder()
        .network(network)
        .identity(identity)
        .bind(bind_patterns)
        .dns(DnsScheme::H3)
        .dns(DnsScheme::Mdns)
        .dns(DnsScheme::System);

    match h3_dns_server {
        Some(h3_dns_server) => {
            builder
                .h3_dns_server(h3_dns_server.to_string().into())
                .build()
                .await
        }
        None => builder.build().await,
    }
    .context(build_registered_endpoint_error::EndpointSnafu)
}

pub async fn build_connector_endpoint(
    network: DhttpNetwork,
    identity: Option<Identity>,
) -> Result<Endpoint, BuildEndpointError> {
    Endpoint::builder()
        .network(network)
        .maybe_identity(identity.map(Arc::new))
        .dns(DnsScheme::H3)
        .dns(DnsScheme::Mdns)
        .dns(DnsScheme::System)
        .build()
        .await
}
