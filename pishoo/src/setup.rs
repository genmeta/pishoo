//! Startup preparation: scan deployments, load configuration and initialize DHTTP.

mod config;
mod discovery;
mod library;
mod network;

use std::{
    io,
    path::{Path, PathBuf},
};

pub use config::{ProxyLocation, ServerConfig, load_server_config};
pub use discovery::valid_lib_id;
pub(crate) use library::discover_libs;
pub use library::validate_lib;
pub use network::network_config;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Clone)]
pub struct Server {
    pub sandbox: crate::sandbox::Sandbox,
    pub name: String,
    pub path: PathBuf,
    pub config: ServerConfig,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn load_server(profile: &dhttp_home::identity::IdentityProfile) -> Option<Server> {
    let name = profile.name().to_owned();
    match load_server_config(profile) {
        Ok(config) => {
            let sandbox =
                crate::sandbox::Sandbox::new(&name, crate::sandbox::SandboxLimits::default())
                    .expect("valid default sandbox limits");
            if let Err(error) = sandbox.load_deployments(profile) {
                eprintln!("skipping libs for {name}: {error}");
            }
            Some(Server {
                sandbox,
                name,
                path: profile.path().to_owned(),
                config,
            })
        }
        Err(error) => {
            eprintln!("skipping server {name}: {error}");
            None
        }
    }
}

pub fn scan(home_dir: &Path) -> Result<Vec<Server>> {
    let home = dhttp_home::DhttpHome::new(home_dir.to_owned());
    Ok(home
        .discover_identity_profiles()?
        .iter()
        .filter_map(load_server)
        .collect())
}

/// Scan configured servers after confirming each identity's credentials load.
pub async fn scan_ready(home: &dhttp_home::DhttpHome) -> Result<Vec<Server>> {
    let mut ready = Vec::new();
    for profile in home.discover_identity_profiles()? {
        match profile.load_identity().await {
            Ok(_) => {
                if let Some(server) = load_server(&profile) {
                    ready.push(server);
                }
            }
            Err(error) => eprintln!("skipping server {}: {error}", profile.name()),
        }
    }
    Ok(ready)
}

/// Prepare every valid server and initialize the one process-wide DHTTP network.
/// Endpoint publication is intentionally outside this preparation phase.
pub async fn startup() -> Result<Vec<Server>> {
    let home = dhttp_home::DhttpHome::load(dhttp_home::HomeScope::User)?;
    let servers = scan_ready(&home).await?;
    if let Some(config) = network_config(
        &servers
            .iter()
            .map(|server| server.config.clone())
            .collect::<Vec<_>>(),
    )? {
        dhttp::DhttpNetwork::init(config).await?;
    }
    Ok(servers)
}

#[cfg(test)]
mod tests;
