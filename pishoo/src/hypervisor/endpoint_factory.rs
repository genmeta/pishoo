//! Endpoint construction helpers for root-managed listeners.
//!
//! RootState owns resource arbitration. The mechanics of constructing DHTTP
//! resolver stacks live here so the registry code does not duplicate Endpoint
//! builder internals.

use std::{sync::Arc, time::Duration};

use dhttp::{
    ddns::resolvers::DnsScheme,
    dquic::{binds::BindPattern, qbase::param::ParameterId, server::ServerQuicConfig},
    endpoint::{BuildEndpointError, Endpoint},
    identity::Identity,
    network::DhttpNetwork,
};
use http::Uri;
use snafu::{ResultExt, Snafu};

const PISHOO_KEEP_ALIVE_DURATION: Duration = Duration::from_secs(2 * 60);
const PISHOO_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(20);
const PISHOO_MAX_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum BuildRegisteredEndpointError {
    #[snafu(display("failed to build endpoint"))]
    Endpoint { source: BuildEndpointError },
}

fn server_quic_config() -> ServerQuicConfig {
    let mut config = dhttp::trust::default_server_quic_config()
        .keep_alive(PISHOO_KEEP_ALIVE_DURATION, PISHOO_HEARTBEAT_INTERVAL);
    config
        .parameters
        .set(ParameterId::MaxIdleTimeout, PISHOO_MAX_IDLE_TIMEOUT)
        .expect("maximum idle timeout is a valid QUIC transport parameter");
    config
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
        .server(server_quic_config())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pishoo_server_uses_two_minute_keep_alive_with_safe_idle_margin() {
        let config = server_quic_config();

        assert_eq!(config.defer_idle_timeout, PISHOO_KEEP_ALIVE_DURATION);
        assert_eq!(config.heartbeat_interval, PISHOO_HEARTBEAT_INTERVAL);
        assert_eq!(
            config
                .parameters
                .get::<Duration>(ParameterId::MaxIdleTimeout),
            Some(PISHOO_MAX_IDLE_TIMEOUT)
        );
    }
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
