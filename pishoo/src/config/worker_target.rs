use std::collections::HashMap;

use gateway::parse::{Node, Value};
pub use nix::unistd::{Gid, Uid, User as ResolvedWorkerTarget};
use snafu::{OptionExt, ResultExt};

use super::{
    ConfigError, EmptyWorkerNameSnafu, GroupNotFoundSnafu, GroupResolveSnafu, InvalidGroupsSnafu,
    InvalidWorkersSnafu, MissingHomeSnafu, UserNotFoundSnafu, UserResolveSnafu,
};

#[derive(Debug, Clone)]
pub struct WorkerTarget {
    pub username: String,
}

const DEFAULT_GROUPS: &[&str] = &["pishoo"];

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

fn parse_configured_workers(pishoo: &Node) -> Result<Vec<WorkerTarget>, ConfigError> {
    match pishoo.get("workers") {
        Some(Value::StringVec(names)) => parse_worker_names(names),
        Some(_) => InvalidWorkersSnafu.fail(),
        None => Ok(Vec::new()),
    }
}

fn parse_groups(pishoo: &Node) -> Result<Vec<String>, ConfigError> {
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

pub(super) fn resolve_all_workers(
    pishoo: &Node,
    has_local_servers: bool,
) -> Result<Vec<WorkerTarget>, ConfigError> {
    let explicit_workers = parse_configured_workers(pishoo)?;
    let groups = parse_groups(pishoo)?;

    let group_members = if groups.is_empty() && explicit_workers.is_empty() && !has_local_servers {
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

pub fn resolve_entry_worker_targets(
    entry_config: &super::EntryConfig,
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
    pub changed: Vec<(ResolvedWorkerTarget, ResolvedWorkerTarget)>,
}

/// Compute a diff between current and next worker targets.
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
