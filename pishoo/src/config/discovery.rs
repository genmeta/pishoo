use std::sync::Arc;

use dhttp_config::identity::IdentityConfig;
use gateway::parse::{document::ConfigNode, error::ConfigLoadFailure};

pub async fn load_identity_servers(
    identity_home: &IdentityConfig,
) -> Result<Vec<Arc<ConfigNode>>, ConfigLoadFailure> {
    let conf_path = identity_home.server_conf_path();
    let registry = gateway::parse::default_registry();
    let parsed = gateway::parse::load_config_file(
        &conf_path,
        &registry,
        gateway::parse::registry::BuildOptions {
            identity_home: Some(identity_home),
        },
    )
    .await?;

    Ok(parsed.root.children_optional("server").to_vec())
}
