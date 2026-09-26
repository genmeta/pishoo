//! Convert persisted listen bits into the shared DHTTP network rule.

use dhttp::{ListenConfig, NetworkConfig, Scope, Scopes};

use super::{Result, config::ServerConfig, invalid};

/// Merge per-server listen bits into the one network scope rule.
pub fn network_config(configs: &[ServerConfig]) -> Result<Option<NetworkConfig>> {
    if configs.iter().any(|config| config.listen > 3) {
        return Err(invalid("listen must be in 0..=3").into());
    }
    let bits = configs.iter().fold(0, |all, config| all | config.listen);
    let scopes: Scopes = match bits {
        0 => return Ok(None),
        1 => Scope::Internal.into(),
        2 => Scope::External.into(),
        _ => Scope::Internal | Scope::External,
    };
    Ok(Some(NetworkConfig {
        listen: vec![ListenConfig::Scope(scopes)],
    }))
}
