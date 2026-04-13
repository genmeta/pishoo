use std::{collections::HashMap, sync::Arc};

use gateway::parse::{Node, Value};
pub use nix::unistd::{Gid, Uid, User as ResolvedWorkerTarget};
use snafu::{OptionExt, ResultExt, Snafu};

mod discovery;

pub use discovery::load_identity_servers;

#[derive(Debug, Clone)]
pub struct WorkerTarget {
    pub username: String,
}

#[derive(Debug, Clone)]
pub struct RootConfig {
    pub pid_file: String,
    pub groups: Vec<String>,
    pub workers: Vec<WorkerTarget>,
}

#[derive(Debug, Clone)]
pub struct EntryConfig {
    pub pid_file: String,
    pub workers: Vec<WorkerTarget>,
    pub local_servers: Vec<Arc<Node>>,
}

#[derive(Debug, Snafu)]
pub enum ConfigError {
    #[snafu(display("pishoo block not found in configuration"))]
    MissingPishoo,

    #[snafu(display("invalid workers directive: expected string list"))]
    InvalidWorkers,

    #[snafu(display("invalid pid directive: expected string"))]
    InvalidPid,

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

fn parse_groups(pishoo: &Arc<Node>) -> Result<Vec<String>, ConfigError> {
    match pishoo.get("groups") {
        Some(Value::StringVec(names)) => Ok(names.clone()),
        Some(_) => InvalidGroupsSnafu.fail(),
        None => Ok(Vec::new()),
    }
}

fn resolve_group_members(group_names: &[String]) -> Result<Vec<WorkerTarget>, ConfigError> {
    let mut targets = Vec::new();
    for group_name in group_names {
        let group = nix::unistd::Group::from_name(group_name)
            .context(GroupResolveSnafu {
                group_name: group_name.clone(),
            })?
            .context(GroupNotFoundSnafu {
                group_name: group_name.clone(),
            })?;
        targets.extend(parse_worker_names(&group.mem)?);
    }
    Ok(targets)
}

const DEFAULT_GROUPS: &[&str] = &["pishoo"];

fn resolve_all_workers(
    pishoo: &Arc<Node>,
    has_local_servers: bool,
) -> Result<Vec<WorkerTarget>, ConfigError> {
    let explicit_workers = parse_configured_workers(pishoo)?;
    let groups = parse_groups(pishoo)?;

    let group_members = if groups.is_empty() && explicit_workers.is_empty() && !has_local_servers {
        // No groups, no workers, and no local servers — use default groups
        resolve_group_members(
            &DEFAULT_GROUPS
                .iter()
                .map(|s| (*s).to_string())
                .collect::<Vec<_>>(),
        )?
    } else if !groups.is_empty() {
        resolve_group_members(&groups)?
    } else {
        Vec::new()
    };

    // Deduplicate by username, preserving order (explicit workers first)
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();
    for worker in explicit_workers.into_iter().chain(group_members) {
        if seen.insert(worker.username.clone()) {
            result.push(worker);
        }
    }
    Ok(result)
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

pub fn parse_root_config(
    root: &std::sync::Arc<gateway::parse::Node>,
) -> Result<RootConfig, ConfigError> {
    let pishoo = first_pishoo_node(root)?;
    let pid_file = parse_pid_file(&pishoo)?;
    let groups = parse_groups(&pishoo)?;
    let workers = resolve_all_workers(&pishoo, false)?;

    Ok(RootConfig {
        pid_file,
        groups,
        workers,
    })
}

pub fn resolve_entry_worker_targets(
    entry_config: &EntryConfig,
) -> Result<Vec<ResolvedWorkerTarget>, ConfigError> {
    resolve_worker_targets(&entry_config.workers)
}

pub fn resolve_worker_targets(
    workers: &[WorkerTarget],
) -> Result<Vec<ResolvedWorkerTarget>, ConfigError> {
    let mut resolved = Vec::with_capacity(workers.len());
    for worker in workers {
        let user = nix::unistd::User::from_name(&worker.username)
            .context(UserResolveSnafu {
                username: worker.username.clone(),
            })?
            .context(UserNotFoundSnafu {
                username: worker.username.clone(),
            })?;
        if user.dir.as_os_str().is_empty() {
            return MissingHomeSnafu {
                username: worker.username.clone(),
            }
            .fail();
        }
        resolved.push(user);
    }
    Ok(resolved)
}

/// Diff result describing which workers are unchanged, added, removed, or changed.
#[derive(Debug)]
pub struct WorkerDiff {
    /// Workers present in both current and next with same (username, uid).
    pub unchanged: Vec<ResolvedWorkerTarget>,
    /// Workers present in next but not in current.
    pub added: Vec<ResolvedWorkerTarget>,
    /// Workers present in current but not in next.
    pub removed: Vec<ResolvedWorkerTarget>,
    /// Workers where username matches but uid changed — these need kill + respawn.
    /// Each element is `(current_target, next_target)` so the kill phase can use
    /// the old UID while the spawn phase uses the new target.
    pub changed: Vec<(ResolvedWorkerTarget, ResolvedWorkerTarget)>,
}

/// Compute a diff between current and next worker targets.
///
/// Matching key: (username, uid) pair. Both must match for a worker to be
/// considered unchanged.
pub fn compute_worker_diff(
    current: &[ResolvedWorkerTarget],
    next: &[ResolvedWorkerTarget],
) -> WorkerDiff {
    let current_map: HashMap<&str, &ResolvedWorkerTarget> =
        current.iter().map(|t| (t.name.as_str(), t)).collect();
    let next_map: HashMap<&str, &ResolvedWorkerTarget> =
        next.iter().map(|t| (t.name.as_str(), t)).collect();

    let mut unchanged = Vec::new();
    let mut added = Vec::new();
    let mut changed = Vec::new();
    let mut removed = Vec::new();

    for next_target in next {
        match current_map.get(next_target.name.as_str()) {
            Some(cur) if cur.uid == next_target.uid => {
                unchanged.push(next_target.clone());
            }
            Some(cur) => {
                // Same username, different uid → kill + respawn
                changed.push(((*cur).clone(), next_target.clone()));
            }
            None => {
                added.push(next_target.clone());
            }
        }
    }

    for cur_target in current {
        if !next_map.contains_key(cur_target.name.as_str()) {
            removed.push(cur_target.clone());
        }
    }

    WorkerDiff {
        unchanged,
        added,
        removed,
        changed,
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, ffi::CString, path::PathBuf, sync::Arc};

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
        let workers = vec![WorkerTarget { username: me.name }];
        let resolved = resolve_worker_targets(&workers).expect("resolve user target");
        assert_eq!(resolved.len(), 1);
        assert!(!resolved[0].dir.as_os_str().is_empty());
    }

    #[test]
    fn parse_entry_config_servers_only() {
        let (cert, key) = create_temp_tls_files();
        let conf = format!(
            "pishoo {{ server {{ listen all 443; server_name demo~; ssl_certificate {}; ssl_certificate_key {}; location / {{ root .; }} }} }}",
            cert.display(),
            key.display()
        );
        let parsed = gateway::parse::parse(conf.as_bytes(), Some(std::path::Path::new(".")))
            .expect("parse config");
        let entry = parse_entry_config(&parsed).expect("parse entry config");
        assert!(entry.workers.is_empty());
        assert_eq!(entry.local_servers.len(), 1);
    }

    #[test]
    fn parse_entry_config_workers_and_servers() {
        let (cert, key) = create_temp_tls_files();
        let conf = format!(
            "pishoo {{ workers alice; server {{ listen all 443; server_name demo~; ssl_certificate {}; ssl_certificate_key {}; location / {{ root .; }} }} }}",
            cert.display(),
            key.display()
        );
        let parsed = gateway::parse::parse(conf.as_bytes(), Some(std::path::Path::new(".")))
            .expect("parse config");
        let entry = parse_entry_config(&parsed).expect("parse entry config");
        assert_eq!(entry.workers.len(), 1);
        assert_eq!(entry.local_servers.len(), 1);
    }

    #[test]
    fn worker_diff_unchanged_ignores_order() {
        let current = vec![
            ResolvedWorkerTarget {
                uid: Uid::from_raw(1),
                gid: Gid::from_raw(11),
                name: "alice".to_string(),
                passwd: CString::default(),
                gecos: CString::default(),
                dir: PathBuf::from("/tmp/alice"),
                shell: PathBuf::new(),
            },
            ResolvedWorkerTarget {
                uid: Uid::from_raw(2),
                gid: Gid::from_raw(22),
                name: "bob".to_string(),
                passwd: CString::default(),
                gecos: CString::default(),
                dir: PathBuf::from("/tmp/bob"),
                shell: PathBuf::new(),
            },
        ];
        let next = vec![
            ResolvedWorkerTarget {
                uid: Uid::from_raw(2),
                gid: Gid::from_raw(22),
                name: "bob".to_string(),
                passwd: CString::default(),
                gecos: CString::default(),
                dir: PathBuf::from("/tmp/bob"),
                shell: PathBuf::new(),
            },
            ResolvedWorkerTarget {
                uid: Uid::from_raw(1),
                gid: Gid::from_raw(11),
                name: "alice".to_string(),
                passwd: CString::default(),
                gecos: CString::default(),
                dir: PathBuf::from("/tmp/alice"),
                shell: PathBuf::new(),
            },
        ];

        let diff = compute_worker_diff(&current, &next);
        assert_eq!(diff.unchanged.len(), 2);
        assert!(diff.added.is_empty());
        assert!(diff.removed.is_empty());
        assert!(diff.changed.is_empty());
    }

    #[test]
    fn worker_diff_detects_add_remove() {
        let current = vec![ResolvedWorkerTarget {
            uid: Uid::from_raw(1),
            gid: Gid::from_raw(11),
            name: "alice".to_string(),
            passwd: CString::default(),
            gecos: CString::default(),
            dir: PathBuf::from("/tmp/alice"),
            shell: PathBuf::new(),
        }];
        let next = vec![ResolvedWorkerTarget {
            uid: Uid::from_raw(2),
            gid: Gid::from_raw(22),
            name: "bob".to_string(),
            passwd: CString::default(),
            gecos: CString::default(),
            dir: PathBuf::from("/tmp/bob"),
            shell: PathBuf::new(),
        }];

        let diff = compute_worker_diff(&current, &next);
        assert!(diff.unchanged.is_empty());
        assert_eq!(diff.added.len(), 1);
        assert_eq!(diff.added[0].name, "bob");
        assert_eq!(diff.removed.len(), 1);
        assert_eq!(diff.removed[0].name, "alice");
        assert!(diff.changed.is_empty());
    }

    #[test]
    fn worker_diff_detects_uid_change() {
        let current = vec![ResolvedWorkerTarget {
            uid: Uid::from_raw(1000),
            gid: Gid::from_raw(1000),
            name: "alice".to_string(),
            passwd: CString::default(),
            gecos: CString::default(),
            dir: PathBuf::from("/tmp/alice"),
            shell: PathBuf::new(),
        }];
        let next = vec![ResolvedWorkerTarget {
            uid: Uid::from_raw(1001),
            gid: Gid::from_raw(1001),
            name: "alice".to_string(),
            passwd: CString::default(),
            gecos: CString::default(),
            dir: PathBuf::from("/tmp/alice"),
            shell: PathBuf::new(),
        }];

        let diff = compute_worker_diff(&current, &next);
        assert!(diff.unchanged.is_empty());
        assert!(diff.added.is_empty());
        assert!(diff.removed.is_empty());
        assert_eq!(diff.changed.len(), 1);
        assert_eq!(diff.changed[0].0.uid, Uid::from_raw(1000));
        assert_eq!(diff.changed[0].1.uid, Uid::from_raw(1001));
        assert_eq!(diff.changed[0].1.name, "alice");
    }
}
