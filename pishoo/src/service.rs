//! Shared service logic for both worker processes and root-local services.
//!
//! The [`run_service`] function is generic over [`ControlPlane`], allowing
//! the exact same code to run in a worker process (using
//! `RemoteControlPlane`) or directly inside the root process (using
//! `LocalControlPlane`).

use std::{collections::HashMap, sync::Arc};

use gateway::{
    control_plane::{ControlPlane, ListenRequest},
    parse::Node,
    reverse::{self, MissingRulePolicy},
};
use h3x::{dhttp::settings::Settings, quic};
use snafu::Report;
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
    /// Router mapping server_name → config node.
    pub router: Arc<HashMap<String, Arc<Node>>>,
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
pub async fn run_service<P: ControlPlane>(
    plane: &P,
    config: &ServiceConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    P::Listener: 'static,
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

        let mut listener = plane
            .listener(request)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        tracing::info!(%server_name, "Listener registered");

        let h3_settings = config.h3_settings.clone();
        let router = config.router.clone();
        let access_rules = config.access_rules.clone();
        let missing_rule_policy = config.missing_rule_policy;

        tasks.spawn(
            async move {
                loop {
                    let conn = match quic::Listen::accept(&mut listener).await {
                        Ok(conn) => conn,
                        Err(error) => {
                            tracing::warn!(
                                %server_name,
                                error = %Report::from_error(&error),
                                "Listener accept error, stopping"
                            );
                            break;
                        }
                    };

                    let sn = server_name.clone();
                    let settings = h3_settings.clone();
                    let r = router.clone();
                    let ar = access_rules.clone();
                    tokio::spawn(
                        async move {
                            if let Err(error) = reverse::handle_single_connection_for_server(
                                conn,
                                sn.clone(),
                                settings,
                                r,
                                ar,
                                missing_rule_policy,
                            )
                            .await
                            {
                                tracing::warn!(
                                    server_name = %sn,
                                    error = %Report::from_error(&error),
                                    "Connection handling failed"
                                );
                            }
                        }
                        .in_current_span(),
                    );
                }
            }
            .in_current_span(),
        );
    }

    // Wait for all server tasks (they run until shutdown).
    while tasks.join_next().await.is_some() {}

    Ok(())
}
