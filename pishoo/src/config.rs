use std::sync::Arc;

use gateway::parse::{document::ConfigNode, error::ConfigQueryError, types::StringConfig};
use snafu::{OptionExt, ResultExt, Snafu};

mod discovery;
pub mod entry;
pub mod root;
pub mod worker_target;

#[cfg(test)]
mod tests;

pub use discovery::load_identity_servers;
pub use entry::{EntryConfig, parse_entry_config};
pub use root::{RootConfig, parse_root_config};
pub use worker_target::{
    ResolvedWorkerTarget, WorkerDiff, WorkerTarget, compute_worker_diff,
    resolve_entry_worker_targets, resolve_worker_targets,
};

#[derive(Debug, Snafu)]
pub enum ConfigError {
    #[snafu(display("pishoo block not found in configuration"))]
    MissingPishoo,

    #[snafu(display("invalid workers directive: expected string list"))]
    InvalidWorkers,

    #[snafu(display("invalid pid directive: expected string"))]
    InvalidPid,

    #[snafu(display("failed to read typed configuration value"))]
    ConfigQuery { source: ConfigQueryError },

    #[snafu(display("invalid groups directive: expected string list"))]
    InvalidGroups,

    #[snafu(display("worker username cannot be empty"))]
    EmptyWorkerName,

    #[snafu(display("failed to resolve users in group `{group_name}`"))]
    GroupResolve {
        group_name: String,
        source: nix::Error,
    },

    #[snafu(display("group `{group_name}` not found"))]
    GroupNotFound { group_name: String },

    #[snafu(display("failed to resolve user `{username}` via system passwd database"))]
    UserNotFound { username: String },

    #[snafu(display("failed to resolve user `{username}`"))]
    UserResolve {
        username: String,
        source: nix::Error,
    },

    #[snafu(display("resolved user `{username}` has no home directory"))]
    MissingHome { username: String },
}

pub const PID_FILE_DEFAULT: &str = "/var/run/pishoo.pid";

fn first_pishoo_node(root: &Arc<ConfigNode>) -> Result<Arc<ConfigNode>, ConfigError> {
    root.children("pishoo")
        .ok()
        .and_then(|nodes| nodes.first().cloned())
        .context(MissingPishooSnafu)
}

fn parse_pid_file(pishoo: &Arc<ConfigNode>) -> Result<String, ConfigError> {
    Ok(pishoo
        .get::<StringConfig>("pid")
        .context(ConfigQuerySnafu)?
        .map(|pid_file| pid_file.0.clone())
        .unwrap_or_else(|| PID_FILE_DEFAULT.to_string()))
}
