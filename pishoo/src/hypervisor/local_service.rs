use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
};

use dhttp::{ddns::PublishOptions, identity::Identity, name::DhttpName};
use gateway::{
    control_plane::ListenRequest,
    error::Whatever,
    parse::{
        document::ConfigNode,
        error::ConfigQueryError,
        types::{
            ListenConfig, Listens, PathConfig, ResolverConfig, ServerIdConfig, ServerNames,
            StringConfig,
        },
    },
};
use snafu::{ResultExt, Snafu, whatever};
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};

use crate::{
    hypervisor::state::RootState, listen::RegisteredEndpoint, service::PreparedServer, tls,
};

#[allow(dead_code)]
struct LocalServerDef {
    server_name: DhttpName<'static>,
    cert_pem: Vec<u8>,
    key_pem: Vec<u8>,
}

pub async fn validate_local_servers(servers: &[Arc<ConfigNode>]) -> Result<(), Whatever> {
    let _ = collect_local_server_defs(servers).await?;
    Ok(())
}

async fn collect_local_server_defs(
    servers: &[Arc<ConfigNode>],
) -> Result<Vec<LocalServerDef>, Whatever> {
    let mut seen_server_names = HashSet::new();
    let mut defs = Vec::new();

    for server in servers {
        let listens = listen_values(server).whatever_context("failed to read local listen")?;
        if listens.is_empty() {
            whatever!("local server missing `listen`");
        }
        let server_names = server
            .require::<ServerNames>("server_name")
            .whatever_context("local server missing `server_name`")?;
        let cert_path = server
            .require::<PathConfig>("ssl_certificate")
            .whatever_context("local server missing `ssl_certificate`")?
            .0
            .clone();
        let key_path = server
            .require::<PathConfig>("ssl_certificate_key")
            .whatever_context("local server missing `ssl_certificate_key`")?
            .0
            .clone();

        let cert_pem = tokio::fs::read(&cert_path).await.whatever_context(format!(
            "failed to read local certificate file `{}`",
            cert_path.display()
        ))?;
        let key_pem = tokio::fs::read(&key_path).await.whatever_context(format!(
            "failed to read local private key file `{}`",
            key_path.display()
        ))?;
        let _ = tls::validate_tls_material(&cert_pem, &key_pem)
            .whatever_context("invalid local tls material")?;

        for configured_name in &server_names.0 {
            let server_name = configured_name.name.clone();
            if !seen_server_names.insert(server_name.clone()) {
                whatever!("duplicate local server_name `{server_name}` in entry config");
            }
            defs.push(LocalServerDef {
                server_name,
                cert_pem: cert_pem.clone(),
                key_pem: key_pem.clone(),
            });
        }
    }

    Ok(defs)
}

// ---------------------------------------------------------------------------
// Service lifecycle management
// ---------------------------------------------------------------------------

/// Handle to a running root-local service, used for shutdown and replacement.
pub struct LocalServiceHandle {
    shutdown: CancellationToken,
    task: AbortOnDropHandle<Vec<PreparedServer<RegisteredEndpoint>>>,
}

impl LocalServiceHandle {
    /// Cancel the running service and recover prepared servers with listeners.
    pub async fn shutdown(self) -> Vec<PreparedServer<RegisteredEndpoint>> {
        self.shutdown.cancel();
        self.task.await.unwrap_or_default()
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum BuildLocalServiceError {
    #[snafu(display("failed to canonicalize local server nodes"))]
    Canonicalize { source: Whatever },

    #[snafu(display("local server missing `{directive}`"))]
    MissingDirective { directive: &'static str },

    #[snafu(display("failed to read typed configuration value"))]
    ConfigQuery { source: ConfigQueryError },

    #[snafu(display("failed to read certificate at `{}`", path.display()))]
    ReadCert {
        path: PathBuf,
        source: std::io::Error,
    },

    #[snafu(display("failed to read private key at `{}`", path.display()))]
    ReadKey {
        path: PathBuf,
        source: std::io::Error,
    },

    #[snafu(display("invalid TLS material"))]
    InvalidTls { source: tls::TlsMaterialError },

    #[snafu(display("invalid server name `{name}`"))]
    InvalidServerName {
        name: String,
        source: dhttp::name::InvalidDhttpName,
    },

    #[snafu(display("failed to load local access rules"))]
    LoadPolicy { source: crate::policy::PolicyError },
}

/// Build a [`ServiceConfig`](crate::service::ServiceConfig) from the
/// root-local server blocks in the entry configuration.
pub async fn build_local_service_config(
    local_servers: &[Arc<ConfigNode>],
) -> Result<crate::service::ServiceConfig, BuildLocalServiceError> {
    let canonicalized = crate::naming::canonicalize_server_nodes(local_servers)
        .context(build_local_service_error::CanonicalizeSnafu)?;

    // Collect the first explicit access_rules URI found across local servers.
    let mut access_rules_uri: Option<String> = None;

    let mut server_configs = Vec::new();
    for server in &canonicalized {
        let listens = listen_values_for_local_service(server)?;
        let server_names = require_local::<ServerNames>(server, "server_name")?;
        let cert_path = require_local::<PathConfig>(server, "ssl_certificate")?
            .0
            .clone();
        let key_path = require_local::<PathConfig>(server, "ssl_certificate_key")?
            .0
            .clone();

        if access_rules_uri.is_none()
            && let Some(uri) = server
                .get::<StringConfig>("access_rules")
                .context(build_local_service_error::ConfigQuerySnafu)?
        {
            access_rules_uri = Some(uri.0.clone());
        }

        let cert_pem = tokio::fs::read(&cert_path)
            .await
            .context(build_local_service_error::ReadCertSnafu { path: &cert_path })?;
        let key_pem = tokio::fs::read(&key_path)
            .await
            .context(build_local_service_error::ReadKeySnafu { path: &key_path })?;
        let (certs, key) = tls::validate_tls_material(&cert_pem, &key_pem)
            .context(build_local_service_error::InvalidTlsSnafu)?;

        // Extract DNS resolver URL from the server node's `dns` directive.
        let dns_resolver_url = server
            .get::<ResolverConfig>("dns")
            .context(build_local_service_error::ConfigQuerySnafu)?
            .map(|resolver| resolver.0.clone());
        let publish_options = PublishOptions {
            server_id: server
                .get::<ServerIdConfig>("server_id")
                .context(build_local_service_error::ConfigQuerySnafu)?
                .map(|id| id.0),
        };

        for configured_name in &server_names.0 {
            server_configs.push(crate::service::ServerConfig {
                listen_request: ListenRequest {
                    identity: Identity::new(
                        configured_name.name.clone().into(),
                        certs.clone(),
                        key.clone_key(),
                    ),
                    bind: listens.clone(),
                    dns_resolver_url: dns_resolver_url.clone(),
                    publish_options,
                },
                server_node: server.clone(),
                access_log_dir: None,
            });
        }
    }

    let local_policy = crate::policy::load_policy_bundle(access_rules_uri.as_deref())
        .await
        .context(build_local_service_error::LoadPolicySnafu)?;
    let access_rules = local_policy.location_rules;

    Ok(crate::service::ServiceConfig {
        servers: server_configs,
        h3_settings: Arc::new(h3x::dhttp::settings::Settings::default()),
        access_rules,
    })
}

/// Spawn the root-local service from configuration. Returns `None` if no
/// local servers are configured.
pub async fn spawn_local_service(
    state: &Arc<RootState>,
    entry_config: &crate::config::EntryConfig,
    existing_listeners: HashMap<DhttpName<'static>, RegisteredEndpoint>,
) -> Result<Option<LocalServiceHandle>, Whatever> {
    if entry_config.local_servers.is_empty() {
        tracing::debug!("no local servers configured");
        // Shut down any leftover listeners from a previous cycle.
        for (name, listener) in existing_listeners {
            tracing::info!(%name, "shutting down listener (no local servers configured)");
            let _ = h3x::quic::Listen::shutdown(&listener).await;
        }
        return Ok(None);
    }

    let config = build_local_service_config(&entry_config.local_servers)
        .await
        .whatever_context("failed to build local service config")?;

    let plane = Arc::new(crate::hypervisor::local_plane::LocalControlPlane::new(
        state.clone(),
    ));
    let shutdown = CancellationToken::new();
    let service_shutdown = shutdown.clone();

    let mut prepared = crate::service::setup_service(&*plane, &config, existing_listeners)
        .await
        .whatever_context("failed to set up local service")?;

    let server_count = prepared.len();
    let h3_settings = config.h3_settings.clone();
    let access_rules = config.access_rules.clone();
    let router_state = gateway::reverse::router::RouterState {
        #[cfg(feature = "sshd")]
        session_spawner: plane.clone(),
    };

    let handle = AbortOnDropHandle::new(tokio::spawn(async move {
        crate::service::run_service(
            &mut prepared,
            &h3_settings,
            &access_rules,
            router_state,
            service_shutdown,
        )
        .await;
        prepared
    }));

    tracing::info!(servers = server_count, "local service started");

    Ok(Some(LocalServiceHandle {
        shutdown,
        task: handle,
    }))
}

/// Replace the running root-local service with a freshly built one.
///
/// Shuts down the old service (if any), recovers reusable listeners, then
/// spawns a new one from the updated entry configuration.
pub async fn replace_local_service(
    state: &Arc<RootState>,
    handle: &mut Option<LocalServiceHandle>,
    entry_config: &crate::config::EntryConfig,
) -> Result<(), Whatever> {
    let existing_listeners = if let Some(old_handle) = handle.take() {
        let config = build_local_service_config(&entry_config.local_servers)
            .await
            .whatever_context("failed to build local service config for diff")?;
        let old_prepared = old_handle.shutdown().await;
        crate::service::collect_reusable_listeners(old_prepared, &config).await
    } else {
        HashMap::new()
    };

    *handle = spawn_local_service(state, entry_config, existing_listeners).await?;
    Ok(())
}

fn listen_values(server: &ConfigNode) -> Result<Vec<Listens>, ConfigQueryError> {
    Ok(server
        .get_all::<ListenConfig>("listen")?
        .into_iter()
        .flat_map(|listen| listen.0.clone())
        .collect())
}

fn listen_values_for_local_service(
    server: &ConfigNode,
) -> Result<Vec<Listens>, BuildLocalServiceError> {
    let listens = listen_values(server).context(build_local_service_error::ConfigQuerySnafu)?;
    if listens.is_empty() {
        return build_local_service_error::MissingDirectiveSnafu {
            directive: "listen",
        }
        .fail();
    }
    Ok(listens)
}

fn require_local<T>(
    server: &ConfigNode,
    directive: &'static str,
) -> Result<Arc<T>, BuildLocalServiceError>
where
    T: gateway::parse::value::ConfigValue,
{
    match server
        .get::<T>(directive)
        .context(build_local_service_error::ConfigQuerySnafu)?
    {
        Some(value) => Ok(value),
        None => build_local_service_error::MissingDirectiveSnafu { directive }.fail(),
    }
}
