//! Worker identity configuration: scan identities and build [`ServiceConfig`].
//!
//! Workers scan the user's `~/.genmeta/` directory for identities, load
//! per-identity pishoo config files, and produce a [`ServiceConfig`] to
//! feed into [`run_service()`](crate::service::run_service).

use std::{collections::HashMap, path::Path, sync::Arc};

use futures::StreamExt;
use gateway::{
    control_plane::ListenRequest,
    error::Whatever,
    parse::{Node, Value},
    reverse::MissingRulePolicy,
};
use genmeta_home::GenmetaHome;
use snafu::{ResultExt, Snafu};

use crate::{
    bind::resolve_bind_uris,
    config::load_identity_servers,
    policy,
    service::{ServerConfig, ServiceConfig},
};

/// Errors during worker configuration loading.
#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum BuildConfigError {
    #[snafu(transparent)]
    Whatever { source: Whatever },
    #[snafu(display("failed to load access rules"))]
    Policy { source: policy::PolicyError },
}

impl snafu::FromString for BuildConfigError {
    type Source = <Whatever as snafu::FromString>::Source;

    fn without_source(message: String) -> Self {
        Whatever::without_source(message).into()
    }

    fn with_source(source: Self::Source, message: String) -> Self {
        Whatever::with_source(source, message).into()
    }
}

/// Build a [`ServiceConfig`] by scanning all identities under the given
/// [`GenmetaHome`], loading their TLS material and pishoo.conf server
/// definitions.
pub async fn build_service_config(
    genmeta_home: &GenmetaHome,
    worker_access_rules_config_path: &Path,
) -> Result<ServiceConfig, BuildConfigError> {
    let device_names = gm_quic::qinterface::device::Devices::global()
        .interfaces()
        .keys()
        .cloned()
        .collect::<Vec<_>>();

    // Collect identity names from the stream.
    let mut identity_names = Vec::new();
    let mut stream = std::pin::pin!(genmeta_home.identities());
    while let Some(result) = stream.next().await {
        match result {
            Ok(name) => identity_names.push(name),
            Err(error) => {
                tracing::warn!(
                    error = %snafu::Report::from_error(&error),
                    "failed to read identity entry, skipping"
                );
            }
        }
    }

    // For each identity, load TLS material + config → build ServerConfigs.
    let mut servers = Vec::new();
    let mut all_server_nodes = Vec::new();
    let mut fallback_entries: HashMap<String, Arc<Node>> = HashMap::new();

    for name in &identity_names {
        let identity_home = match genmeta_home.load_identity(name.borrow()).await {
            Ok(home) => home,
            Err(error) => {
                tracing::warn!(
                    %name,
                    error = %snafu::Report::from_error(&error),
                    "failed to load identity home, skipping"
                );
                continue;
            }
        };

        // Load TLS material.
        let ssl = match identity_home.identity().await {
            Ok(id) => id,
            Err(error) => {
                tracing::warn!(
                    %name,
                    error = %snafu::Report::from_error(&error),
                    "failed to load TLS material, skipping"
                );
                continue;
            }
        };

        // Always register a fallback router entry for the identity name.
        let server_name = name.as_full().to_string();
        fallback_entries
            .entry(server_name)
            .or_insert_with(|| Arc::new(Node::new(Value::ValueMap(HashMap::new()))));

        // Load per-identity pishoo.conf if present.
        let conf_path = identity_home.path().join("pishoo.conf");
        let identity_server_nodes = if conf_path.is_file() {
            match load_identity_servers(&conf_path).await {
                Ok(nodes) => nodes,
                Err(error) => {
                    tracing::warn!(
                        %name,
                        error = %snafu::Report::from_error(&error),
                        "failed to load identity config, skipping"
                    );
                    continue;
                }
            }
        } else {
            Vec::new()
        };

        // Extract bind addresses from server nodes.
        let mut binds: HashMap<String, Vec<String>> = HashMap::new();
        for server_node in &identity_server_nodes {
            if let (Some(Value::ServerName(server_names)), Some(Value::Listen(listens))) =
                (server_node.get("server_name"), server_node.get("listen"))
            {
                for sn in server_names {
                    binds.insert(sn.name.clone(), resolve_bind_uris(listens, &device_names));
                }
            }
        }
        all_server_nodes.extend(identity_server_nodes);

        // Build the listen request using the identity's server name as primary bind key,
        // falling back to default bind if not explicitly configured.
        let bind = binds.remove(name.as_full()).unwrap_or_default();
        if bind.is_empty() {
            tracing::warn!(%name, "no resolved bind URIs, skipping listener request");
            continue;
        }

        let listen_request = ListenRequest {
            identity: ssl,
            bind,
        };

        servers.push(ServerConfig {
            listen_request,
            server_node: Arc::new(Node::new(Value::ValueMap(HashMap::new()))),
        });
    }

    // Build the router from all collected server nodes.
    let mut router = gateway::reverse::build_router_for_servers(&all_server_nodes)
        .as_ref()
        .clone();
    for (entry_name, node) in fallback_entries {
        router.entry(entry_name).or_insert(node);
    }

    // Load worker access rules policy.
    let access_rules = load_worker_access_rules(worker_access_rules_config_path)
        .await
        .context(build_config_error::PolicySnafu)?;

    // TODO: parse HTTP/3 settings from config once schema is finalized.
    let h3_settings = Arc::new(h3x::dhttp::settings::Settings::default());

    Ok(ServiceConfig {
        servers,
        h3_settings,
        router: Arc::new(router),
        access_rules,
        missing_rule_policy: MissingRulePolicy::Deny,
    })
}

/// Load access rules from the worker's `pishoo.conf`.
async fn load_worker_access_rules(
    conf_path: &Path,
) -> Result<Arc<firewall_db::base::matcher::LocationRulesMatcher>, policy::PolicyError> {
    if !conf_path.exists() {
        return Ok(Arc::new(
            firewall_db::base::matcher::LocationRulesMatcher::default(),
        ));
    }

    let raw = match tokio::fs::read(conf_path).await {
        Ok(raw) => raw,
        Err(_) => {
            return Ok(Arc::new(
                firewall_db::base::matcher::LocationRulesMatcher::default(),
            ));
        }
    };

    let parsed = match gateway::parse::parse(&raw, conf_path.parent()) {
        Ok(parsed) => parsed,
        Err(_) => {
            return Ok(Arc::new(
                firewall_db::base::matcher::LocationRulesMatcher::default(),
            ));
        }
    };

    let uri = parsed
        .get("pishoo")
        .and_then(|v| match v {
            Value::Nodes(nodes) => nodes.first(),
            _ => None,
        })
        .and_then(|node| match node.get("access_rules") {
            Some(Value::String(uri)) => Some(uri.clone()),
            _ => None,
        });

    let bundle = policy::load_policy_bundle(uri.as_deref()).await?;
    Ok(bundle.location_rules)
}
