use std::time::Duration;

pub mod config;
pub mod manager;

pub use config::{StunBindConfig, StunNodeConfig};
pub use manager::StunServerManager;

const STUN_RECONCILE_INTERVAL: Duration = Duration::from_secs(30);
const STUN_PUBLISH_INTERVAL: Duration = Duration::from_secs(20);
pub const STUN_DOMAIN: &str = dhttp::endpoint::STUN_DOMAIN;
