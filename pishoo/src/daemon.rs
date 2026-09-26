use std::{
    collections::{BTreeMap, HashSet},
    path::PathBuf,
    sync::{Arc, RwLock},
    time::Duration,
};

use axum::response::IntoResponse;
use dhttp_home::{DhttpHome, identity::IdentityProfile};
use http_body_util::BodyExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use crate::{
    Body, Error, Result, TerminalPolicy,
    routes::build_router,
    sandbox::Sandbox,
    setup::{ServerConfig, load_server_config, network_config},
    terminal::TerminalManager,
    wasm::{Lib, LibPolicy, Runtime},
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonConfig {
    pub state_dir: PathBuf,
    #[serde(default)]
    pub terminal: TerminalPolicy,
}
struct Daemon {
    home: DhttpHome,
    servers: BTreeMap<String, Server>,
    listeners: JoinSet<(String, Result<()>)>,
    runtime: Arc<Runtime>,
    terminal: Arc<TerminalManager>,
}
struct Server {
    profile: IdentityProfile,
    endpoint: dhttp::Endpoint,
    config: ServerConfig,
    access: Arc<access_control::AccessService>,
    router: Arc<RwLock<axum::Router>>,
    libs: BTreeMap<String, Arc<Lib>>,
    runtime: Arc<Runtime>,
    sandbox: Arc<Sandbox>,
    terminal: Arc<TerminalManager>,
    cancel: CancellationToken,
}

include!("daemon/lifecycle.rs");
include!("daemon/server.rs");
include!("daemon/deployment.rs");

#[cfg(test)]
#[path = "../tests/unit/daemon.rs"]
mod tests;
