use std::sync::Arc;

use gateway::parse::{Node, Value};

use super::{
    ConfigError, first_pishoo_node, parse_pid_file,
    worker_target::{WorkerTarget, resolve_all_workers},
};

#[derive(Debug, Clone)]
pub struct EntryConfig {
    pub pid_file: String,
    pub workers: Vec<WorkerTarget>,
    pub local_servers: Vec<Arc<Node>>,
}

fn parse_local_servers(pishoo: &Arc<Node>) -> Vec<Arc<Node>> {
    match pishoo.get("server") {
        Some(Value::Nodes(nodes)) => nodes.clone(),
        Some(_) | None => Vec::new(),
    }
}

pub fn parse_entry_config(root: &Arc<Node>) -> Result<EntryConfig, ConfigError> {
    let pishoo = first_pishoo_node(root)?;
    let pid_file = parse_pid_file(&pishoo)?;
    let local_servers = parse_local_servers(&pishoo);
    let workers = resolve_all_workers(&pishoo, !local_servers.is_empty())?;

    Ok(EntryConfig {
        pid_file,
        workers,
        local_servers,
    })
}
