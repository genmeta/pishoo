use std::path::PathBuf;

use gateway::parse::{document::ConfigNode, types::StringList};
use snafu::ResultExt;

use super::{
    ConfigError, first_pishoo_node, parse_pid_file,
    worker_target::{WorkerTarget, resolve_all_workers},
};

#[derive(Debug, Clone)]
pub struct RootConfig {
    pub pid_file: PathBuf,
    pub groups: Vec<String>,
    pub workers: Vec<WorkerTarget>,
}

pub fn parse_root_config(root: &std::sync::Arc<ConfigNode>) -> Result<RootConfig, ConfigError> {
    let pishoo = first_pishoo_node(root)?;
    let pid_file = parse_pid_file(&pishoo)?;

    let groups = pishoo
        .get::<StringList>("groups")
        .context(super::ConfigQuerySnafu)?
        .map(|groups| groups.0.clone())
        .unwrap_or_default();

    let workers = resolve_all_workers(&pishoo)?;

    Ok(RootConfig {
        pid_file,
        groups,
        workers,
    })
}
