//! Worker identity configuration: scan identities and build [`ServiceConfig`].
//!
//! Workers scan the user's `~/.dhttp/` directory for identities, load
//! per-identity server.conf files, and produce a [`ServiceConfig`] to
//! feed into [`run_service()`](crate::service::run_service).

use std::{collections::HashMap, sync::Arc};

use dhttp_home::DhttpHome;
use futures::StreamExt;
use gateway::{
    control_plane::ListenRequest,
    error::Whatever,
    parse::{Listens, Node, Value},
};
use snafu::{ResultExt, Snafu};

use crate::{
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
/// [`DhttpHome`], loading their TLS material and server.conf definitions.
pub async fn build_service_config(
    dhttp_home: &DhttpHome,
) -> Result<ServiceConfig, BuildConfigError> {
    // Collect identity names from the stream.
    let mut identity_names = Vec::new();
    let mut stream = std::pin::pin!(dhttp_home.identities());
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
    // Collect the first explicit access_rules URI found, or use default.
    let mut access_rules_uri: Option<String> = None;

    for name in &identity_names {
        let identity_home = match dhttp_home.load_identity(name.borrow()).await {
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

        // Always register a fallback server node for the identity name.

        // Load per-identity server.conf if present.
        let conf_path = identity_home.join("server.conf");
        let identity_server_nodes = if conf_path.is_file() {
            match load_identity_servers(&identity_home).await {
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

        // Pick up access_rules from server nodes if not yet found.
        if access_rules_uri.is_none() {
            for server_node in &identity_server_nodes {
                if let Some(Value::String(uri)) = server_node.get("access_rules") {
                    access_rules_uri = Some(uri.clone());
                    break;
                }
            }
        }

        // Default access_rules: IDENTITY_HOME/db/access.db
        if access_rules_uri.is_none() {
            let default_db = identity_home.path().join("db/access.db");
            if default_db.is_file() {
                access_rules_uri = Some(format!("sqlite://{}?mode=ro", default_db.display()));
            }
        }

        // Extract bind specifications and find the matching server node.
        let mut binds: HashMap<String, Vec<Listens>> = HashMap::new();
        let mut server_nodes_by_name: HashMap<String, Arc<Node>> = HashMap::new();
        for server_node in &identity_server_nodes {
            if let (Some(Value::ServerName(server_names)), Some(Value::Listen(listens))) =
                (server_node.get("server_name"), server_node.get("listen"))
            {
                for sn in server_names {
                    binds.insert(sn.name.clone(), listens.clone());
                    server_nodes_by_name.insert(sn.name.clone(), server_node.clone());
                }
            }
        }

        // Build the listen request using the identity's server name as primary bind key,
        // falling back to default bind if not explicitly configured.
        let primary_name = name.as_full().to_owned();
        let bind = binds.remove(&primary_name).unwrap_or_default();
        if bind.is_empty() {
            tracing::warn!(%name, "no listen specifications, skipping listener request");
            continue;
        }

        let listen_request = ListenRequest {
            identity: ssl,
            bind,
        };

        // Use the parsed server node (with location blocks etc.) instead of
        // an empty node — this is what carries proxy/file/location config
        // through to the service layer.
        let server_node = server_nodes_by_name
            .remove(&primary_name)
            .unwrap_or_else(|| Arc::new(Node::new(Value::ValueMap(HashMap::new()))));

        servers.push(ServerConfig {
            listen_request,
            server_node,
        });
    }

    // Load worker access rules policy.
    let access_rules_bundle = policy::load_policy_bundle(access_rules_uri.as_deref())
        .await
        .context(build_config_error::PolicySnafu)?;

    // TODO: parse HTTP/3 settings from config once schema is finalized.
    let h3_settings = Arc::new(h3x::dhttp::settings::Settings::default());

    Ok(ServiceConfig {
        servers,
        h3_settings,
        access_rules: access_rules_bundle.location_rules,
    })
}
