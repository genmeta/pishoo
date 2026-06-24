use std::{path::PathBuf, sync::Arc};

use gateway::parse::document::ConfigNode;

use super::{
    ConfigError, first_pishoo_node, parse_pid_file,
    worker_target::{
        AccountDirectory, SystemAccountDirectory, WorkerDiscoveryMode, WorkerTarget,
        resolve_all_workers_with_directory,
    },
};

#[derive(Debug, Clone)]
pub struct EntryConfig {
    pub pid_file: PathBuf,
    pub workers: Vec<WorkerTarget>,
    pub local_servers: Vec<Arc<ConfigNode>>,
}

fn parse_local_servers(pishoo: &Arc<ConfigNode>) -> Vec<Arc<ConfigNode>> {
    pishoo.children_optional("server").to_vec()
}

pub fn parse_entry_config(root: &Arc<ConfigNode>) -> Result<EntryConfig, ConfigError> {
    parse_entry_config_with_mode(root, WorkerDiscoveryMode::ExplicitConfig)
}

pub fn parse_entry_config_with_mode(
    root: &Arc<ConfigNode>,
    mode: WorkerDiscoveryMode,
) -> Result<EntryConfig, ConfigError> {
    let directory = SystemAccountDirectory;
    parse_entry_config_with_directory_and_mode(root, &directory, mode)
}

#[cfg(test)]
pub(super) fn parse_entry_config_with_directory<D: AccountDirectory>(
    root: &Arc<ConfigNode>,
    directory: &D,
) -> Result<EntryConfig, ConfigError> {
    parse_entry_config_with_directory_and_mode(root, directory, WorkerDiscoveryMode::ExplicitConfig)
}

pub(super) fn parse_entry_config_with_directory_and_mode<D: AccountDirectory>(
    root: &Arc<ConfigNode>,
    directory: &D,
    mode: WorkerDiscoveryMode,
) -> Result<EntryConfig, ConfigError> {
    let pishoo = first_pishoo_node(root)?;
    let pid_file = parse_pid_file(&pishoo)?;
    let local_servers = parse_local_servers(&pishoo);
    let workers = resolve_all_workers_with_directory(&pishoo, directory, mode)?;

    Ok(EntryConfig {
        pid_file,
        workers,
        local_servers,
    })
}
