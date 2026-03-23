use std::{path::PathBuf, sync::Arc};

use gateway::parse::{Node, Value};
use nix::unistd::{Gid, Uid};
use snafu::{OptionExt, ResultExt, Snafu};

mod discovery;
mod validate;

pub use discovery::{discover_entry_servers, discover_worker_servers, load_identity_servers};
pub use validate::{ValidationSummary, validate_entry_tree};

#[derive(Debug, Clone)]
pub struct WorkerTarget {
    pub username: String,
}

#[derive(Debug, Clone)]
pub struct RootConfig {
    pub pid_file: String,
    pub workers: Vec<WorkerTarget>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeShape {
    Direct,
    Supervisor,
    Mixed,
}

#[derive(Debug, Clone)]
pub struct EntryConfig {
    pub pid_file: String,
    pub workers: Vec<WorkerTarget>,
    pub local_access_rules_uri: Option<String>,
    pub local_servers: Vec<Arc<Node>>,
    pub shape: RuntimeShape,
}

#[derive(Debug, Snafu)]
pub enum ConfigError {
    #[snafu(display("pishoo block not found in configuration"))]
    MissingPishoo,

    #[snafu(display("invalid workers directive: expected string list"))]
    InvalidWorkers,

    #[snafu(display("invalid pid directive: expected string"))]
    InvalidPid,

    #[snafu(display("invalid access_rules directive: expected string"))]
    InvalidAccessRules,

    #[snafu(display("configuration must define either `workers` or at least one `server`"))]
    MissingServersOrWorkers,

    #[snafu(display("worker username cannot be empty"))]
    EmptyWorkerName,

    #[snafu(display("failde to reolsver users in user group `{WORKER_GROUP}`"))]
    GroupResolve { source: nix::Error },

    #[snafu(display(
        "failde to reolsver users in user group `{WORKER_GROUP}` as user group not found"
    ))]
    GroupNotFound,

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

const WORKER_GROUP: &str = "pishoo";

#[derive(Debug, Clone)]
pub struct ResolvedWorkerTarget {
    pub uid: Uid,
    pub gid: Gid,
    pub username: String,
    pub home: PathBuf,
}

pub const PID_FILE_DEFAULT: &str = "/var/run/pishoo.pid";

fn first_pishoo_node(root: &Arc<Node>) -> Result<Arc<Node>, ConfigError> {
    if let Some(Value::Nodes(nodes)) = root.get("pishoo") {
        nodes.first().cloned().context(MissingPishooSnafu)
    } else {
        MissingPishooSnafu.fail()
    }
}

fn parse_pid_file(pishoo: &Arc<Node>) -> Result<String, ConfigError> {
    match pishoo.get("pid") {
        Some(Value::String(pid_file)) => Ok(pid_file.clone()),
        Some(_) => InvalidPidSnafu.fail(),
        None => Ok(PID_FILE_DEFAULT.to_string()),
    }
}

fn parse_worker_names(names: &[String]) -> Result<Vec<WorkerTarget>, ConfigError> {
    names
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
        .collect()
}

fn parse_configured_workers(pishoo: &Arc<Node>) -> Result<Vec<WorkerTarget>, ConfigError> {
    match pishoo.get("workers") {
        Some(Value::StringVec(names)) => parse_worker_names(names),
        Some(_) => InvalidWorkersSnafu.fail(),
        None => Ok(Vec::new()),
    }
}

fn parse_access_rules_uri(pishoo: &Arc<Node>) -> Result<Option<String>, ConfigError> {
    match pishoo.get("access_rules") {
        Some(Value::String(uri)) => Ok(Some(uri.clone())),
        Some(_) => InvalidAccessRulesSnafu.fail(),
        None => Ok(None),
    }
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
    let workers = parse_configured_workers(&pishoo)?;
    let local_access_rules_uri = parse_access_rules_uri(&pishoo)?;
    let local_servers = parse_local_servers(&pishoo);

    let shape = match (workers.is_empty(), local_servers.is_empty()) {
        (false, false) => RuntimeShape::Mixed,
        (false, true) => RuntimeShape::Supervisor,
        (true, false) => RuntimeShape::Direct,
        (true, true) => return MissingServersOrWorkersSnafu.fail(),
    };

    Ok(EntryConfig {
        pid_file,
        workers,
        local_access_rules_uri,
        local_servers,
        shape,
    })
}

pub fn parse_root_config(
    root: &std::sync::Arc<gateway::parse::Node>,
) -> Result<RootConfig, ConfigError> {
    let pishoo = first_pishoo_node(root)?;
    let pid_file = parse_pid_file(&pishoo)?;

    let workers = match pishoo.get("workers") {
        Some(Value::StringVec(names)) => parse_worker_names(names)?,
        Some(_) => return InvalidWorkersSnafu.fail(),
        None => parse_worker_names(
            &nix::unistd::Group::from_name(WORKER_GROUP)
                .context(GroupResolveSnafu)?
                .context(GroupNotFoundSnafu)?
                .mem,
        )?,
    };

    Ok(RootConfig { pid_file, workers })
}

pub fn resolve_entry_worker_targets(
    entry_config: &EntryConfig,
) -> Result<Vec<ResolvedWorkerTarget>, ConfigError> {
    if entry_config.workers.is_empty() {
        return Ok(Vec::new());
    }

    resolve_worker_targets(&RootConfig {
        pid_file: entry_config.pid_file.clone(),
        workers: entry_config.workers.clone(),
    })
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
            uid: user.uid,
            gid: user.gid,
            username: user.name,
            home,
        });
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, path::PathBuf, sync::Arc};

    use gateway::parse::Node;

    use super::*;

    fn create_temp_tls_files() -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "pishoo-config-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock should be after epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).expect("create temp tls dir");
        let cert = base.join("server.pem");
        let key = base.join("server.key");
        std::fs::write(&cert, b"dummy cert").expect("write cert fixture");
        std::fs::write(&key, b"dummy key").expect("write key fixture");
        (cert, key)
    }

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
    }

    #[test]
    fn parse_entry_config_direct_mode() {
        let (cert, key) = create_temp_tls_files();
        let conf = format!(
            "pishoo {{ server {{ listen all 443; server_name demo~; ssl_certificate {}; ssl_certificate_key {}; location / {{ root .; }} }} }}",
            cert.display(),
            key.display()
        );
        let parsed = gateway::parse::parse(conf.as_bytes(), Some(std::path::Path::new(".")))
            .expect("parse config");
        let entry = parse_entry_config(&parsed).expect("parse entry config");
        assert_eq!(entry.shape, RuntimeShape::Direct);
        assert!(entry.workers.is_empty());
        assert_eq!(entry.local_servers.len(), 1);
    }

    #[test]
    fn parse_entry_config_mixed_mode() {
        let (cert, key) = create_temp_tls_files();
        let conf = format!(
            "pishoo {{ workers alice; server {{ listen all 443; server_name demo~; ssl_certificate {}; ssl_certificate_key {}; location / {{ root .; }} }} }}",
            cert.display(),
            key.display()
        );
        let parsed = gateway::parse::parse(conf.as_bytes(), Some(std::path::Path::new(".")))
            .expect("parse config");
        let entry = parse_entry_config(&parsed).expect("parse entry config");
        assert_eq!(entry.shape, RuntimeShape::Mixed);
        assert_eq!(entry.workers.len(), 1);
        assert_eq!(entry.local_servers.len(), 1);
    }
}
