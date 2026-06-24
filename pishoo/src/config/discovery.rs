use std::sync::Arc;

use dhttp::home::identity::IdentityProfile;
use gateway::parse::{document::ConfigNode, error::ConfigLoadFailure};

pub async fn load_identity_servers(
    identity_profile: &IdentityProfile,
) -> Result<Vec<Arc<ConfigNode>>, ConfigLoadFailure> {
    let conf_path = identity_profile.server_conf_path();
    let registry = gateway::parse::default_registry();
    let parsed = gateway::parse::load_config_file(
        &conf_path,
        &registry,
        gateway::parse::registry::BuildOptions {
            dhttp_home: None,
            identity_profile: Some(identity_profile),
        },
    )
    .await?;

    Ok(parsed.root.children_optional("server").to_vec())
}
