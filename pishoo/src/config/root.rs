use gateway::parse::Node;

use super::{
    ConfigError, first_pishoo_node, parse_pid_file,
    worker_target::{WorkerTarget, resolve_all_workers},
};

#[derive(Debug, Clone)]
pub struct RootConfig {
    pub pid_file: String,
    pub groups: Vec<String>,
    pub workers: Vec<WorkerTarget>,
}

pub fn parse_root_config(root: &std::sync::Arc<Node>) -> Result<RootConfig, ConfigError> {
    let pishoo = first_pishoo_node(root)?;
    let pid_file = parse_pid_file(&pishoo)?;

    let groups = match pishoo.get("groups") {
        Some(gateway::parse::Value::StringVec(names)) => names.clone(),
        Some(_) => return super::InvalidGroupsSnafu.fail(),
        None => Vec::new(),
    };

    let workers = resolve_all_workers(&pishoo, false)?;

    Ok(RootConfig {
        pid_file,
        groups,
        workers,
    })
}
