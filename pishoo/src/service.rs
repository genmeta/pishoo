//! Shared service logic for both worker processes and root-local services.
//!
//! [`setup_service`] registers QUIC listeners via the control plane (reusing
//! existing ones when possible). [`run_service`] builds the HTTP/3 service
//! stack and runs accept loops. When cancelled, listeners are recovered into
//! the [`PreparedServer`]s so they can be reused on the next reload without
//! tearing down underlying QUIC bindings.

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use axum::middleware::from_fn_with_state;
use dhttp::{identity::Identity, name::DhttpName};
use gateway::{
    control_plane::{ControlPlane, ListenRequest},
    parse::document::ConfigNode,
    reverse::{
        access_control::{AccessControlState, access_control},
        access_log::{AccessLogState, access_log},
        body_adapter::BodyAdapterLayer,
        log::AccessLogWriter,
        router::{NginxRouter, RouterState},
    },
};
use h3x::{
    connection::ConnectionBuilder,
    dhttp::settings::Settings,
    endpoint::H3Endpoint,
    hyper::server::TowerService,
    quic::{self, Listen as _},
};
use snafu::Report;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tower::ServiceBuilder;
use tracing::Instrument;

/// Configuration for a single server within a service.
pub struct ServerConfig {
    /// The listen request to send to the control plane.
    pub listen_request: ListenRequest,
    /// Parsed nginx-style server configuration node.
    pub server_node: Arc<ConfigNode>,
    /// Directory for per-identity access logs (e.g. `~/.dhttp/{name}/logs/`).
    /// `None` disables access logging for this server.
    pub access_log_dir: Option<PathBuf>,
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
    pub access_rules: Arc<dhttp_access::db::base::matcher::LocationRulesMatcher>,
}

/// A server whose QUIC listener has been registered and is ready to run.
///
/// Holds the listener and associated metadata. The listener is temporarily
/// taken during [`run_service`] and recovered on cancellation so it can be
/// reused across reloads.
pub struct PreparedServer<L: quic::Listen> {
    /// Server name (SNI).
    pub server_name: DhttpName<'static>,
    /// Original listen request, retained for diffing on reload.
    pub listen_request: ListenRequest,
    /// The QUIC listener. `Some` when idle, `None` while `run_service` is active.
    pub listener: Option<L>,
    /// Parsed nginx-style server configuration node.
    pub server_node: Arc<ConfigNode>,
    /// Access log directory (or `None` to disable).
    pub access_log_dir: Option<PathBuf>,
}

/// Register QUIC listeners for all servers in the config.
///
/// Reuses listeners from `existing_listeners` (keyed by server_name) when
/// available. For servers not in the map, a new listener is requested via
/// the control plane.
///
/// Any listeners remaining in `existing_listeners` after processing all
/// servers are shut down (they belong to servers removed from the config).
pub async fn setup_service<P: ControlPlane + 'static>(
    plane: &P,
    config: &ServiceConfig,
    mut existing_listeners: HashMap<DhttpName<'static>, P::Listener>,
) -> Result<Vec<PreparedServer<P::Listener>>, P::ListenError>
where
    P::Listener: 'static,
{
    let mut result = Vec::new();

    for server_config in &config.servers {
        let server_name = DhttpName::try_from(
            server_config
                .listen_request
                .identity
                .name()
                .as_full()
                .to_owned(),
        )
        .expect("listen request identity must be a dhttp name");

        let listener = if let Some(listener) = existing_listeners.remove(&server_name) {
            tracing::debug!(%server_name, "reusing existing listener");
            listener
        } else {
            let request = ListenRequest {
                identity: Identity::new(
                    server_config.listen_request.identity.name().clone(),
                    server_config.listen_request.identity.certs().to_vec(),
                    server_config.listen_request.identity.key().clone_key(),
                ),
                bind: server_config.listen_request.bind.clone(),
                dns_resolver_url: server_config.listen_request.dns_resolver_url.clone(),
                publish_options: server_config.listen_request.publish_options,
            };
            let listener = plane.listener(request).await?;
            tracing::debug!(%server_name, "listener registered");
            listener
        };

        result.push(PreparedServer {
            server_name,
            listen_request: ListenRequest {
                identity: Identity::new(
                    server_config.listen_request.identity.name().clone(),
                    server_config.listen_request.identity.certs().to_vec(),
                    server_config.listen_request.identity.key().clone_key(),
                ),
                bind: server_config.listen_request.bind.clone(),
                dns_resolver_url: server_config.listen_request.dns_resolver_url.clone(),
                publish_options: server_config.listen_request.publish_options,
            },
            listener: Some(listener),
            server_node: server_config.server_node.clone(),
            access_log_dir: server_config.access_log_dir.clone(),
        });
    }

    // Shut down listeners for servers removed from the config.
    for (name, listener) in existing_listeners {
        tracing::info!(%name, "shutting down removed server listener");
        if let Err(error) = listener.shutdown().await {
            tracing::warn!(
                %name,
                error = %Report::from_error(&error),
                "failed to shut down removed listener"
            );
        }
    }

    Ok(result)
}

/// Run the service accept loop for prepared servers.
///
/// Builds the HTTP/3 service stack for each server, then runs accept loops
/// until `shutdown` is triggered or all servers stop. On return, recovered
/// listeners are placed back into each [`PreparedServer`]'s `listener` field.
pub async fn run_service<L>(
    prepared: &mut [PreparedServer<L>],
    h3_settings: &Arc<Settings>,
    access_rules: &Arc<dhttp_access::db::base::matcher::LocationRulesMatcher>,
    router_state: RouterState,
    shutdown: CancellationToken,
) where
    L: quic::Listen + 'static,
    L::Error: Send,
    L::Connection: 'static,
    <L::Connection as quic::WithLocalAgent>::LocalAgent: Send + Sync,
    <L::Connection as quic::WithRemoteAgent>::RemoteAgent: Send + Sync,
{
    let server_count = prepared.len();

    if server_count == 0 {
        tracing::info!(server_count = 0, "worker ready");
        shutdown.cancelled().await;
        return;
    }

    let mut tasks: JoinSet<(DhttpName<'static>, L)> = JoinSet::new();

    for server in prepared.iter_mut() {
        let listener = server
            .listener
            .take()
            .expect("listener must be present before run_service");
        let server_name = server.server_name.clone();

        // Build the service stack: BodyAdapter → AccessLog → AccessControl → NginxRouter
        let locations = server.server_node.children_optional("location").to_vec();

        let nginx_router = NginxRouter::new(locations, router_state.clone());
        let access_state = AccessControlState {
            access_rules: access_rules.clone(),
            server_name: Arc::from(server_name.as_full()),
        };

        let access_log_writer = match &server.access_log_dir {
            Some(dir) => match AccessLogWriter::new(dir.clone()) {
                Ok(writer) => {
                    tracing::debug!(
                        %server_name,
                        dir = %dir.display(),
                        "access log writer created"
                    );
                    writer
                }
                Err(error) => {
                    tracing::warn!(
                        %server_name,
                        %error,
                        "failed to create access log writer, access logging disabled"
                    );
                    AccessLogWriter::disabled()
                }
            },
            None => AccessLogWriter::disabled(),
        };
        let access_log_state = AccessLogState {
            writer: access_log_writer,
        };

        let service_stack = ServiceBuilder::new()
            .layer(BodyAdapterLayer)
            .layer(from_fn_with_state(access_log_state, access_log))
            .layer(from_fn_with_state(access_state, access_control))
            .service(nginx_router);

        // Build H3 connection builder with configured settings
        let builder = ConnectionBuilder::new(h3_settings.clone());
        #[cfg(feature = "sshd")]
        let builder = builder.protocol(dssh::protocol::Ssh3ProtocolFactory);

        let mut endpoint = H3Endpoint::builder()
            .quic(listener)
            .builder(Arc::new(builder))
            .build();
        let service = TowerService(service_stack);

        let cancel = shutdown.clone();
        tasks.spawn(
            async move {
                tokio::select! {
                    result = endpoint.serve(service) => {
                        match result {
                            Ok(()) => {
                                tracing::warn!(%server_name, "server stopped");
                            }
                            Err(error) => {
                                tracing::warn!(
                                    %server_name,
                                    error = %Report::from_error(&error),
                                    "server stopped"
                                );
                            }
                        }
                    }
                    () = cancel.cancelled() => {
                        // Intentionally not calling endpoint.shutdown() —
                        // preserve the underlying QUIC listener bindings.
                    }
                }
                let listener = endpoint.into_quic();
                (server_name, listener)
            }
            .in_current_span(),
        );
    }

    tracing::info!(server_count, "worker ready");

    // Wait for all server tasks to complete.
    let mut recovered: HashMap<DhttpName<'static>, L> = HashMap::new();
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok((name, listener)) => {
                recovered.insert(name, listener);
            }
            Err(error) => {
                tracing::warn!(error = %error, "server task panicked");
            }
        }
    }

    // Put recovered listeners back into their PreparedServer slots.
    for server in prepared.iter_mut() {
        if let Some(listener) = recovered.remove(&server.server_name) {
            server.listener = Some(listener);
        }
    }
}

/// Collect reusable listeners from prepared servers, shutting down those
/// whose network config (listen request) has changed.
pub async fn collect_reusable_listeners<L: quic::Listen>(
    old_prepared: Vec<PreparedServer<L>>,
    new_config: &ServiceConfig,
) -> HashMap<DhttpName<'static>, L> {
    let new_requests: HashMap<DhttpName<'static>, &ListenRequest> = new_config
        .servers
        .iter()
        .map(|sc| {
            let name = DhttpName::try_from(sc.listen_request.identity.name().as_full().to_owned())
                .expect("listen request identity must be a dhttp name");
            (name, &sc.listen_request)
        })
        .collect();

    let mut reusable = HashMap::new();

    for mut server in old_prepared {
        let Some(listener) = server.listener.take() else {
            continue;
        };

        let should_reuse = new_requests
            .get(&server.server_name)
            .is_some_and(|new_req| **new_req == server.listen_request);

        if should_reuse {
            tracing::debug!(
                server_name = %server.server_name,
                "listener eligible for reuse"
            );
            reusable.insert(server.server_name, listener);
        } else {
            tracing::info!(
                server_name = %server.server_name,
                "shutting down changed/removed server listener"
            );
            if let Err(error) = listener.shutdown().await {
                tracing::warn!(
                    server_name = %server.server_name,
                    error = %Report::from_error(&error),
                    "failed to shut down listener"
                );
            }
        }
    }

    reusable
}
