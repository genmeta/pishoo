use std::{collections::HashSet, path::PathBuf, sync::Arc};

use gateway::{
    control_plane::{Identity, ListenRequest},
    error::Whatever,
    parse::{Node, Value},
};
use snafu::{ResultExt, Snafu, whatever};
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};

use crate::{root::state::RootState, tls};

#[allow(dead_code)]
struct LocalServerDef {
    server_name: String,
    cert_pem: Vec<u8>,
    key_pem: Vec<u8>,
}

pub async fn validate_local_servers(servers: &[Arc<Node>]) -> Result<(), Whatever> {
    let _ = collect_local_server_defs(servers).await?;
    Ok(())
}

async fn collect_local_server_defs(servers: &[Arc<Node>]) -> Result<Vec<LocalServerDef>, Whatever> {
    let mut seen_server_names = HashSet::new();
    let mut defs = Vec::new();

    for server in servers {
        let Some(Value::Listen(listens)) = server.get("listen") else {
            whatever!("local server missing `listen`");
        };
        if listens.is_empty() {
            whatever!("local server has empty listen specification");
        }
        let Some(Value::ServerName(server_names)) = server.get("server_name") else {
            whatever!("local server missing `server_name`");
        };
        let server_names = server_names.clone();
        let Some(Value::Path(cert_path)) = server.get("ssl_certificate") else {
            whatever!("local server missing `ssl_certificate`");
        };
        let cert_path = cert_path.clone();
        let Some(Value::Path(key_path)) = server.get("ssl_certificate_key") else {
            whatever!("local server missing `ssl_certificate_key`");
        };
        let key_path = key_path.clone();

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

        for configured_name in server_names {
            let server_name = configured_name.name;
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
    task: AbortOnDropHandle<()>,
}

impl LocalServiceHandle {
    pub async fn shutdown(self) {
        self.shutdown.cancel();
        let _ = self.task.await;
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum BuildLocalServiceError {
    #[snafu(display("failed to canonicalize local server nodes"))]
    Canonicalize { source: Whatever },

    #[snafu(display("local server missing `{directive}`"))]
    MissingDirective { directive: &'static str },

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
        source: dhttp_home::identity::InvalidName,
    },

    #[snafu(display("failed to load local access rules"))]
    LoadPolicy { source: crate::policy::PolicyError },
}

/// Build a [`ServiceConfig`](crate::service::ServiceConfig) from the
/// root-local server blocks in the entry configuration.
pub async fn build_local_service_config(
    local_servers: &[Arc<Node>],
) -> Result<crate::service::ServiceConfig, BuildLocalServiceError> {
    let canonicalized = crate::naming::canonicalize_server_nodes(local_servers)
        .context(build_local_service_error::CanonicalizeSnafu)?;

    // Collect the first explicit access_rules URI found across local servers.
    let mut access_rules_uri: Option<String> = None;

    let mut server_configs = Vec::new();
    for server in &canonicalized {
        let Some(Value::Listen(listens)) = server.get("listen") else {
            return build_local_service_error::MissingDirectiveSnafu {
                directive: "listen",
            }
            .fail();
        };
        let listens = listens.clone();
        let Some(Value::ServerName(server_names)) = server.get("server_name") else {
            return build_local_service_error::MissingDirectiveSnafu {
                directive: "server_name",
            }
            .fail();
        };
        let server_names = server_names.clone();
        let Some(Value::Path(cert_path)) = server.get("ssl_certificate") else {
            return build_local_service_error::MissingDirectiveSnafu {
                directive: "ssl_certificate",
            }
            .fail();
        };
        let cert_path = cert_path.clone();
        let Some(Value::Path(key_path)) = server.get("ssl_certificate_key") else {
            return build_local_service_error::MissingDirectiveSnafu {
                directive: "ssl_certificate_key",
            }
            .fail();
        };
        let key_path = key_path.clone();

        if access_rules_uri.is_none()
            && let Some(Value::String(uri)) = server.get("access_rules")
        {
            access_rules_uri = Some(uri.clone());
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
        let dns_resolver_url = match server.get("dns") {
            Some(Value::Resolver(url)) => Some(url.to_string()),
            _ => None,
        };

        for configured_name in server_names {
            let name = dhttp_home::identity::Name::try_from_str(configured_name.name.clone())
                .context(build_local_service_error::InvalidServerNameSnafu {
                    name: &configured_name.name,
                })?;
            server_configs.push(crate::service::ServerConfig {
                listen_request: ListenRequest {
                    identity: Identity::new(name, certs.clone(), key.clone_key()),
                    bind: listens.clone(),
                    dns_resolver_url: dns_resolver_url.clone(),
                },
                server_node: server.clone(),
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
) -> Result<Option<LocalServiceHandle>, Whatever> {
    if entry_config.local_servers.is_empty() {
        return Ok(None);
    }

    let config = build_local_service_config(&entry_config.local_servers)
        .await
        .whatever_context("failed to build local service config")?;

    let plane = Arc::new(crate::root::local_plane::LocalControlPlane::new(
        state.clone(),
    ));
    let shutdown = CancellationToken::new();
    let service_shutdown = shutdown.clone();

    let service_handle = crate::service::setup_service(plane, &config)
        .await
        .whatever_context("failed to set up local service")?;

    let handle = AbortOnDropHandle::new(tokio::spawn(async move {
        crate::service::run_service(service_handle, service_shutdown).await;
    }));

    Ok(Some(LocalServiceHandle {
        shutdown,
        task: handle,
    }))
}

/// Replace the running root-local service with a freshly built one.
///
/// Shuts down the old service (if any), then spawns a new one from the
/// updated entry configuration.
pub async fn replace_local_service(
    state: &Arc<RootState>,
    handle: &mut Option<LocalServiceHandle>,
    entry_config: &crate::config::EntryConfig,
) -> Result<(), Whatever> {
    if let Some(old_handle) = handle.take() {
        old_handle.shutdown().await;
    }

    *handle = spawn_local_service(state, entry_config).await?;
    Ok(())
}
