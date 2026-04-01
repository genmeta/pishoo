//! Shared service logic for both worker processes and root-local services.
//!
//! The [`run_service`] function is generic over [`ControlPlane`], allowing
//! the exact same code to run in a worker process (using
//! `RemoteControlPlane`) or directly inside the root process (using
//! `LocalControlPlane`).

use std::sync::Arc;

use gateway::{
    control_plane::{ControlPlane, ListenRequest},
    parse::{Node, Value},
    reverse::{
        MissingRulePolicy,
        middleware::{AccessControlLayer, AccessLogLayer},
        router::NginxRouter,
    },
};
use h3x::{
    connection::ConnectionBuilder, dhttp::settings::Settings, hyper::server::TowerService, quic,
    server::Servers,
};
use snafu::Report;
use tokio_util::sync::CancellationToken;
use tower::ServiceBuilder;
use tracing::Instrument;

/// Configuration for a single server within a service.
pub struct ServerConfig {
    /// The listen request to send to the control plane.
    pub listen_request: ListenRequest,
    /// Parsed nginx-style server configuration node.
    pub server_node: Arc<Node>,
}

/// Configuration for a service instance (shared between worker and root-local).
///
/// Parsed from identity-level config files by the respective config modules.
pub struct ServiceConfig {
    /// Servers to register and serve.
    pub servers: Vec<ServerConfig>,
    /// HTTP/3 settings for all servers.
    pub h3_settings: Arc<Settings>,
    /// Access control rules.
    pub access_rules: Arc<firewall_db::base::matcher::LocationRulesMatcher>,
    /// Policy for requests that don't match any access rule.
    pub missing_rule_policy: MissingRulePolicy,
}

/// Run the service loop: register listeners and connectors via the control
/// plane, then serve HTTP/3 reverse proxy and (optionally) forward proxy.
///
/// This is the unified entry point for both worker processes and root-local
/// services. The `P` type parameter determines whether requests go over
/// remoc RPC (worker) or directly in-process (root-local).
///
/// Takes an `Arc<P>` so the control plane can be shared with SSH session
/// handlers (when the `sshd` feature is enabled).
pub async fn run_service<P: ControlPlane + 'static>(
    plane: Arc<P>,
    config: &ServiceConfig,
    shutdown: CancellationToken,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    P::Listener: 'static,
    <P::Listener as quic::Listen>::Error: Send,
    <P::Listener as quic::Listen>::Connection: 'static,
    <<P::Listener as quic::Listen>::Connection as quic::WithLocalAgent>::LocalAgent: Send + Sync,
    <<P::Listener as quic::Listen>::Connection as quic::WithRemoteAgent>::RemoteAgent: Send + Sync,
    P::ListenError: 'static,
{
    let mut tasks = tokio::task::JoinSet::new();

    for server_config in &config.servers {
        let request = ListenRequest {
            identity: gateway::control_plane::Identity::new(
                server_config.listen_request.identity.name().clone(),
                server_config.listen_request.identity.certs().to_vec(),
                server_config.listen_request.identity.key().clone_key(),
            ),
            bind: server_config.listen_request.bind.clone(),
        };
        let server_name = request.identity.name().as_full().to_owned();

        let listener = plane
            .listener(request)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        tracing::info!(%server_name, "Listener registered");

        // Extract location blocks from this server's config node
        let locations = match server_config.server_node.get("location") {
            Some(Value::Nodes(locations)) => locations.clone(),
            _ => Vec::new(),
        };

        // Build the service stack: AccessLog → AccessControl → NginxRouter
        let nginx_router = NginxRouter::new(
            locations,
            gateway::reverse::router::RouterState {
                #[cfg(feature = "sshd")]
                session_spawner: plane.clone(),
            },
        );
        let service_stack = ServiceBuilder::new()
            .layer(AccessLogLayer)
            .layer(AccessControlLayer::new(
                config.access_rules.clone(),
                config.missing_rule_policy,
            ))
            .service(nginx_router);

        // Build H3 connection builder with configured settings
        let builder = ConnectionBuilder::new(config.h3_settings.clone());
        #[cfg(feature = "sshd")]
        let builder = builder.protocol(genmeta_ssh::protocol::Ssh3ProtocolFactory);

        let mut servers = Servers::from_quic_listener()
            .listener(listener)
            .service(TowerService(service_stack))
            .builder(Arc::new(builder))
            .build();
        let server_shutdown = shutdown.clone();

        tasks.spawn(
            async move {
                tokio::select! {
                    error = servers.run() => {
                        tracing::warn!(
                            %server_name,
                            error = %Report::from_error(&error),
                            "server stopped"
                        );
                    }
                    () = server_shutdown.cancelled() => {
                        if let Err(error) = servers.shutdown().await {
                            tracing::warn!(
                                %server_name,
                                error = %Report::from_error(&error),
                                "server shutdown failed"
                            );
                        }
                    }
                }
            }
            .in_current_span(),
        );
    }

    tracing::info!(server_count = config.servers.len(), "worker ready");

    // Wait for all server tasks (they run until shutdown).
    while tasks.join_next().await.is_some() {}

    Ok(())
}
