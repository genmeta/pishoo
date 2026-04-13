//! Shared service logic for both worker processes and root-local services.
//!
//! The [`run_service`] function is generic over [`ControlPlane`], allowing
//! the exact same code to run in a worker process (using
//! `RemoteControlPlane`) or directly inside the root process (using
//! `LocalControlPlane`).

use std::sync::Arc;

use axum::middleware::{from_fn, from_fn_with_state};
use gateway::{
    control_plane::{ControlPlane, ListenRequest},
    parse::{Node, Value},
    reverse::{
        middleware::{AccessControlState, BodyAdapterLayer, access_control, access_log},
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
}

/// Run the service loop: register listeners and connectors via the control
/// plane, then serve HTTP/3 reverse proxy and (optionally) forward proxy.
///
/// This is the unified entry point for both worker processes and root-local
/// services. The `P` type parameter determines whether requests go over
/// remoc RPC (worker) or directly in-process (root-local).
///
/// A closure that spawns one server's accept loop onto a [`JoinSet`].
type ServerSpawner = Box<dyn FnOnce(&mut tokio::task::JoinSet<()>, CancellationToken) + Send>;

/// Handle returned by [`setup_service`], containing server task spawners
/// ready to run.
///
/// Each server is represented as a named closure that, when called, spawns
/// the accept loop onto a [`JoinSet`].
pub struct ServiceHandle {
    spawners: Vec<ServerSpawner>,
    server_count: usize,
}

/// Set up a service: register all listeners via the control plane and build
/// the HTTP/3 service stack for each server. Does **not** start the accept
/// loop — call [`run_service`] with the returned handle to begin serving.
///
/// In dry-run / test mode, callers can drop the handle after setup succeeds
/// to validate configuration without serving traffic.
pub async fn setup_service<P: ControlPlane + 'static>(
    plane: Arc<P>,
    config: &ServiceConfig,
) -> Result<ServiceHandle, P::ListenError>
where
    P::Listener: 'static,
    <P::Listener as quic::Listen>::Error: Send,
    <P::Listener as quic::Listen>::Connection: 'static,
    <<P::Listener as quic::Listen>::Connection as quic::WithLocalAgent>::LocalAgent: Send + Sync,
    <<P::Listener as quic::Listen>::Connection as quic::WithRemoteAgent>::RemoteAgent: Send + Sync,
    P::ListenError: 'static,
{
    let mut spawners: Vec<ServerSpawner> = Vec::new();

    for server_config in &config.servers {
        let request = ListenRequest {
            identity: gateway::control_plane::Identity::new(
                server_config.listen_request.identity.name().clone(),
                server_config.listen_request.identity.certs().to_vec(),
                server_config.listen_request.identity.key().clone_key(),
            ),
            bind: server_config.listen_request.bind.clone(),
            dns_resolver_url: server_config.listen_request.dns_resolver_url.clone(),
        };
        let server_name = request.identity.name().as_full().to_owned();

        let listener = plane.listener(request).await?;

        tracing::info!(%server_name, "listener registered");

        // Extract location blocks from this server's config node
        let locations = match server_config.server_node.get("location") {
            Some(Value::Nodes(locations)) => locations.clone(),
            _ => Vec::new(),
        };

        // Build the service stack: BodyAdapter → AccessLog → AccessControl → NginxRouter
        let nginx_router = NginxRouter::new(
            locations,
            gateway::reverse::router::RouterState {
                #[cfg(feature = "sshd")]
                session_spawner: plane.clone(),
            },
        );
        let access_state = AccessControlState {
            access_rules: config.access_rules.clone(),
            server_name: Arc::from(server_name.as_str()),
        };
        let service_stack = ServiceBuilder::new()
            .layer(BodyAdapterLayer)
            .layer(from_fn(access_log))
            .layer(from_fn_with_state(access_state, access_control))
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

        spawners.push(Box::new(
            move |tasks: &mut tokio::task::JoinSet<()>, shutdown: CancellationToken| {
                let server_shutdown = shutdown;
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
            },
        ));
    }

    let server_count = config.servers.len();
    Ok(ServiceHandle {
        spawners,
        server_count,
    })
}

/// Run the service accept loop for a previously set up service handle.
///
/// This consumes the handle and runs until `shutdown` is triggered or all
/// server tasks complete.
pub async fn run_service(handle: ServiceHandle, shutdown: CancellationToken) {
    let mut tasks = tokio::task::JoinSet::new();

    for spawner in handle.spawners {
        spawner(&mut tasks, shutdown.clone());
    }

    tracing::info!(server_count = handle.server_count, "worker ready");

    if handle.server_count == 0 {
        // No servers to run — wait for shutdown rather than returning immediately.
        shutdown.cancelled().await;
    } else {
        // Wait for all server tasks (they run until shutdown).
        while tasks.join_next().await.is_some() {}
    }
}
