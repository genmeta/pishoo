//! Endpoint construction helpers for root-managed listeners.
//!
//! RootState owns resource arbitration. The mechanics of constructing DHTTP
//! resolver stacks live here so the registry code does not duplicate Endpoint
//! builder internals.

use std::{
    io,
    path::PathBuf,
    sync::{Arc, OnceLock},
};

use dhttp::{
    ddns::{
        BuildQuicEndpointWithDnsError, DhttpDnsPlan, quic_endpoint_builder_with_dns,
        resolvers::DnsScheme,
    },
    dquic::{
        QuicEndpoint,
        binds::BindPattern,
        log::{QLog, handy::LegacySeqLogger},
        server::ServerQuicConfig,
    },
    endpoint::{BuildEndpointError, Endpoint, InvalidEndpointPartsError},
    h3x::endpoint::H3Endpoint,
    identity::Identity,
    network::DhttpNetwork,
    trust,
};
use http::Uri;
use snafu::{ResultExt, Snafu};

const PISHOO_QLOG_DIR_ENV: &str = "PISHOO_QLOG_DIR";
static SERVER_QLOGGER: OnceLock<Arc<dyn QLog + Send + Sync>> = OnceLock::new();

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum BuildRegisteredEndpointError {
    #[snafu(display("failed to build endpoint dns"))]
    EndpointDns {
        source: BuildQuicEndpointWithDnsError,
    },
    #[snafu(display("invalid endpoint parts"))]
    InvalidParts { source: InvalidEndpointPartsError },
    #[snafu(display("failed to create qlog directory {}", path.display()))]
    CreateQlogDirectory { path: PathBuf, source: io::Error },
}

fn enable_server_qlog_from_env(
    config: &mut ServerQuicConfig,
) -> Result<(), BuildRegisteredEndpointError> {
    let Some(qlog_dir) = std::env::var_os(PISHOO_QLOG_DIR_ENV).map(PathBuf::from) else {
        return Ok(());
    };

    std::fs::create_dir_all(&qlog_dir).map_err(|source| {
        BuildRegisteredEndpointError::CreateQlogDirectory {
            path: qlog_dir.clone(),
            source,
        }
    })?;

    let qlogger = SERVER_QLOGGER.get_or_init(|| {
        tracing::info!(path = %qlog_dir.display(), "pishoo QUIC qlog enabled");
        Arc::new(LegacySeqLogger::new(qlog_dir))
    });
    config.qlogger = qlogger.clone();
    Ok(())
}

pub async fn build_registered_endpoint(
    identity: Arc<Identity>,
    network: DhttpNetwork,
    bind_patterns: Arc<Vec<BindPattern>>,
    h3_dns_server: Option<Uri>,
) -> Result<Endpoint, BuildRegisteredEndpointError> {
    let mut dns_plan = DhttpDnsPlan::new();
    dns_plan.push_dns(DnsScheme::H3);
    dns_plan.push_dns(DnsScheme::Mdns);
    dns_plan.push_dns(DnsScheme::System);

    let mut server_config = trust::default_server_quic_config();
    enable_server_qlog_from_env(&mut server_config)?;
    let raw_network = network.network().clone();
    let builder = quic_endpoint_builder_with_dns(
        |resolver| {
            let raw_network = raw_network.clone();
            let identity = identity.clone();
            let bind_patterns = bind_patterns.clone();
            let server_config = server_config.clone();
            async move {
                QuicEndpoint::builder()
                    .network(raw_network)
                    .identity(identity)
                    .resolver(resolver)
                    .client(trust::default_client_quic_config())
                    .server(server_config)
                    .bind(bind_patterns)
                    .build()
                    .await
            }
        },
        &dns_plan,
    );

    let (quic, publishers) = match h3_dns_server {
        Some(h3_dns_server) => {
            builder
                .h3_dns_server(h3_dns_server.to_string().into())
                .build()
                .await
        }
        None => builder.build().await,
    }
    .context(build_registered_endpoint_error::EndpointDnsSnafu)?;
    let h3 = Arc::new(H3Endpoint::new(quic));

    Endpoint::from_parts(h3, publishers, network)
        .context(build_registered_endpoint_error::InvalidPartsSnafu)
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
