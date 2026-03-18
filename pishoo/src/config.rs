use std::path::PathBuf;

use gateway::parse::Value;
use snafu::{OptionExt, ResultExt, Snafu};

#[derive(Debug, Clone)]
pub struct WorkerTarget {
    pub username: String,
}

#[derive(Debug, Clone)]
pub struct RootConfig {
    pub pid_file: String,
    pub workers: Vec<WorkerTarget>,
}

#[derive(Debug, Snafu)]
pub enum ConfigError {
    #[snafu(display("pishoo block not found in configuration"))]
    MissingPishoo,

    #[snafu(display("invalid workers directive: expected string list"))]
    InvalidWorkers,

    #[snafu(display("invalid pid directive: expected string"))]
    InvalidPid,

    #[snafu(display("worker username cannot be empty"))]
    EmptyWorkerName,

    #[snafu(display("failed to resolve user `{username}` via system passwd database"))]
    UserNotFound { username: String },

    #[snafu(display("failed to resolve user `{username}`: {source}"))]
    UserResolve {
        username: String,
        source: nix::Error,
    },

    #[snafu(display("resolved user `{username}` has no home directory"))]
    MissingHome { username: String },
}

#[derive(Debug, Clone)]
pub struct ResolvedWorkerTarget {
    pub uid: u32,
    pub gid: u32,
    pub username: String,
    pub home: PathBuf,
    pub log_dir: PathBuf,
}

pub const PID_FILE_DEFAULT: &str = "/var/run/pishoo.pid";

pub fn parse_root_config(
    root: &std::sync::Arc<gateway::parse::Node>,
) -> Result<RootConfig, ConfigError> {
    let pishoo = if let Some(Value::Nodes(nodes)) = root.get("pishoo") {
        nodes.first().cloned().context(MissingPishooSnafu)?
    } else {
        return MissingPishooSnafu.fail();
    };

    let pid_file = match pishoo.get("pid") {
        Some(Value::String(pid_file)) => pid_file.clone(),
        Some(_) => return InvalidPidSnafu.fail(),
        None => PID_FILE_DEFAULT.to_string(),
    };

    let workers = match pishoo.get("workers") {
        Some(Value::StringVec(names)) => names
            .iter()
            .map(|name| {
                let trimmed = name.trim();
                if trimmed.is_empty() {
                    return EmptyWorkerNameSnafu.fail();
                }
                Ok(WorkerTarget {
                    username: trimmed.to_string(),
                })
            })
            .collect::<Result<Vec<_>, ConfigError>>()?,
        Some(_) => return InvalidWorkersSnafu.fail(),
        None => vec![],
    };

    Ok(RootConfig { pid_file, workers })
}

pub fn resolve_worker_targets(
    config: &RootConfig,
) -> Result<Vec<ResolvedWorkerTarget>, ConfigError> {
    let mut resolved = Vec::with_capacity(config.workers.len());
    for worker in &config.workers {
        let user = nix::unistd::User::from_name(&worker.username)
            .context(UserResolveSnafu {
                username: worker.username.clone(),
            })?
            .context(UserNotFoundSnafu {
                username: worker.username.clone(),
            })?;
        let home = user.dir.clone();
        if home.as_os_str().is_empty() {
            return MissingHomeSnafu {
                username: worker.username.clone(),
            }
            .fail();
        }
        resolved.push(ResolvedWorkerTarget {
            uid: user.uid.as_raw(),
            gid: user.gid.as_raw(),
            username: user.name,
            log_dir: home.join(".genmeta/pishoo/logs"),
            home,
        });
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use gateway::parse::Node;

    use super::*;

    #[test]
    fn invalid_workers_type_is_rejected() {
        let worker_node = Arc::new(Node::new(Value::ValueMap(HashMap::from([(
            "workers".to_string(),
            Value::Boolean(true),
        )]))));
        let root = Arc::new(Node::new(Value::ValueMap(HashMap::from([(
            "pishoo".to_string(),
            Value::Nodes(vec![worker_node]),
        )]))));
        let err = parse_root_config(&root).expect_err("workers must be a string list");
        assert!(matches!(err, ConfigError::InvalidWorkers));
    }

    #[test]
    fn extracts_pid_and_workers() {
        let conf = b"pishoo { pid /tmp/pishoo-test.pid; workers alice bob; }";
        let parsed = gateway::parse::parse(conf, None).expect("parse config");
        let root = parse_root_config(&parsed).expect("parse root config");
        assert_eq!(root.pid_file, "/tmp/pishoo-test.pid");
        assert_eq!(root.workers.len(), 2);
        assert_eq!(root.workers[0].username, "alice");
        assert_eq!(root.workers[1].username, "bob");
    }

    #[test]
    fn parse_workers_from_root_config() {
        let conf = b"pishoo { workers alice bob; }";
        let parsed = gateway::parse::parse(conf, None).expect("parse config");
        let root = parse_root_config(&parsed).expect("parse root config");
        assert_eq!(root.pid_file, PID_FILE_DEFAULT);
        assert_eq!(root.workers.len(), 2);
        assert_eq!(root.workers[0].username, "alice");
        assert_eq!(root.workers[1].username, "bob");
    }

    #[test]
    fn resolve_existing_user_target() {
        let me = nix::unistd::User::from_uid(nix::unistd::getuid())
            .expect("resolve current uid")
            .expect("current user exists");
        let cfg = RootConfig {
            pid_file: PID_FILE_DEFAULT.to_string(),
            workers: vec![WorkerTarget { username: me.name }],
        };
        let resolved = resolve_worker_targets(&cfg).expect("resolve user target");
        assert_eq!(resolved.len(), 1);
        assert!(!resolved[0].home.as_os_str().is_empty());
        assert_eq!(
            resolved[0].log_dir,
            resolved[0].home.join(".genmeta/pishoo/logs")
        );
    }
}
