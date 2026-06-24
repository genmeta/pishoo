use std::collections::HashMap;

use gateway::parse::{document::ConfigNode, types::StringList};
pub use nix::unistd::{Gid, Uid, User as ResolvedWorkerTarget};
use snafu::{OptionExt, ResultExt};

use super::{
    ConfigError, ConfigQuerySnafu, EmptyWorkerNameSnafu, GroupNotFoundSnafu, GroupResolveSnafu,
    MissingHomeSnafu, PrimaryGroupUserResolveSnafu, UserNotFoundSnafu, UserResolveSnafu,
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

fn parse_configured_workers(pishoo: &ConfigNode) -> Result<Vec<WorkerTarget>, ConfigError> {
    match pishoo
        .get::<StringList>("workers")
        .context(ConfigQuerySnafu)?
    {
        Some(names) => parse_worker_names(&names.0),
        None => Ok(Vec::new()),
    }
}

fn parse_groups(pishoo: &ConfigNode) -> Result<Vec<String>, ConfigError> {
    Ok(pishoo
        .get::<StringList>("groups")
        .context(ConfigQuerySnafu)?
        .map(|names| names.0.clone())
        .unwrap_or_default())
}

#[derive(Debug, Clone)]
pub(super) struct AccountGroup {
    pub(super) name: String,
    pub(super) gid: Gid,
    pub(super) members: Vec<String>,
}

pub(super) trait AccountDirectory {
    fn group_by_name(&self, group_name: &str) -> Result<Option<AccountGroup>, ConfigError>;

    fn primary_group_usernames(
        &self,
        group_name: &str,
        gid: Gid,
    ) -> Result<Vec<String>, ConfigError>;
}

pub(super) struct SystemAccountDirectory;

impl AccountDirectory for SystemAccountDirectory {
    fn group_by_name(&self, group_name: &str) -> Result<Option<AccountGroup>, ConfigError> {
        Ok(nix::unistd::Group::from_name(group_name)
            .context(GroupResolveSnafu {
                group_name: group_name.to_string(),
            })?
            .map(|group| AccountGroup {
                name: group.name,
                gid: group.gid,
                members: group.mem,
            }))
    }

    fn primary_group_usernames(
        &self,
        group_name: &str,
        gid: Gid,
    ) -> Result<Vec<String>, ConfigError> {
        Ok(enumerate_passwd_users(group_name)?
            .into_iter()
            .filter(|user| user.gid == gid)
            .map(|user| user.name)
            .collect())
    }
}

struct PasswdIterationGuard;

impl Drop for PasswdIterationGuard {
    fn drop(&mut self) {
        unsafe { libc::endpwent() };
    }
}

fn enumerate_passwd_users(group_name: &str) -> Result<Vec<ResolvedWorkerTarget>, ConfigError> {
    unsafe { libc::setpwent() };
    let _guard = PasswdIterationGuard;
    let mut users = Vec::new();

    loop {
        nix::errno::Errno::clear();
        let passwd = unsafe { libc::getpwent() };
        if passwd.is_null() {
            let errno = nix::errno::Errno::last();
            if errno == nix::errno::Errno::from_raw(0) {
                return Ok(users);
            }
            return Result::<Vec<ResolvedWorkerTarget>, nix::errno::Errno>::Err(errno).context(
                PrimaryGroupUserResolveSnafu {
                    group_name: group_name.to_string(),
                },
            );
        }

        users.push(ResolvedWorkerTarget::from(unsafe { &*passwd }));
    }
}

fn resolve_group_members_with_directory<D: AccountDirectory>(
    directory: &D,
    group_name: &str,
) -> Result<Option<Vec<WorkerTarget>>, ConfigError> {
    let Some(group) = directory.group_by_name(group_name)? else {
        return Ok(None);
    };

    let mut names = group.members;
    names.extend(directory.primary_group_usernames(&group.name, group.gid)?);
    Ok(Some(parse_worker_names(&names)?))
}

fn resolve_explicit_group_members<D: AccountDirectory>(
    directory: &D,
    group_names: &[String],
) -> Result<Vec<WorkerTarget>, ConfigError> {
    let mut targets = Vec::new();
    for group_name in group_names {
        let members = resolve_group_members_with_directory(directory, group_name)?.context(
            GroupNotFoundSnafu {
                group_name: group_name.clone(),
            },
        )?;
        targets.extend(members);
    }
    Ok(targets)
}

fn resolve_default_group_members<D: AccountDirectory>(
    directory: &D,
) -> Result<Vec<WorkerTarget>, ConfigError> {
    let mut targets = Vec::new();
    for group_name in DEFAULT_GROUPS {
        match resolve_group_members_with_directory(directory, group_name)? {
            Some(members) => targets.extend(members),
            None => tracing::warn!(
                "default pishoo worker group not found; continuing without default workers"
            ),
        }
    }
    Ok(targets)
}

pub(super) fn resolve_all_workers(pishoo: &ConfigNode) -> Result<Vec<WorkerTarget>, ConfigError> {
    let directory = SystemAccountDirectory;
    resolve_all_workers_with_directory(pishoo, &directory)
}

pub(super) fn resolve_all_workers_with_directory<D: AccountDirectory>(
    pishoo: &ConfigNode,
    directory: &D,
) -> Result<Vec<WorkerTarget>, ConfigError> {
    let explicit_workers = parse_configured_workers(pishoo)?;
    let groups = parse_groups(pishoo)?;

    let group_members = if groups.is_empty() && explicit_workers.is_empty() {
        resolve_default_group_members(directory)?
    } else if !groups.is_empty() {
        resolve_explicit_group_members(directory, &groups)?
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use gateway::parse::document::ConfigNode;

    use super::*;

    #[derive(Default)]
    struct FakeAccountDirectory {
        groups: HashMap<String, AccountGroup>,
        primary_users: HashMap<libc::gid_t, Vec<String>>,
    }

    impl FakeAccountDirectory {
        fn with_group(mut self, name: &str, gid: libc::gid_t, members: &[&str]) -> Self {
            self.groups.insert(
                name.to_string(),
                AccountGroup {
                    name: name.to_string(),
                    gid: Gid::from_raw(gid),
                    members: members.iter().map(|member| (*member).to_string()).collect(),
                },
            );
            self
        }

        fn with_primary_users(mut self, gid: libc::gid_t, users: &[&str]) -> Self {
            self.primary_users
                .insert(gid, users.iter().map(|user| (*user).to_string()).collect());
            self
        }
    }

    impl AccountDirectory for FakeAccountDirectory {
        fn group_by_name(&self, group_name: &str) -> Result<Option<AccountGroup>, ConfigError> {
            Ok(self.groups.get(group_name).cloned())
        }

        fn primary_group_usernames(
            &self,
            _group_name: &str,
            gid: Gid,
        ) -> Result<Vec<String>, ConfigError> {
            Ok(self
                .primary_users
                .get(&gid.as_raw())
                .cloned()
                .unwrap_or_default())
        }
    }

    fn first_pishoo(conf: &str) -> std::sync::Arc<ConfigNode> {
        let parsed = gateway::parse::parse_config_str_for_test(conf).expect("parse config");
        parsed
            .root
            .children("pishoo")
            .expect("pishoo block should exist")
            .first()
            .expect("pishoo block should not be empty")
            .clone()
    }

    #[test]
    fn default_group_missing_is_not_a_config_error() {
        let pishoo = first_pishoo("pishoo { }");
        let directory = FakeAccountDirectory::default();

        let workers = resolve_all_workers_with_directory(&pishoo, &directory)
            .expect("missing default pishoo group should warn and continue");

        assert!(workers.is_empty());
    }

    #[test]
    fn explicit_group_missing_remains_a_config_error() {
        let pishoo = first_pishoo("pishoo { groups pishoo; }");
        let directory = FakeAccountDirectory::default();

        let error = resolve_all_workers_with_directory(&pishoo, &directory)
            .expect_err("explicit missing group should fail");

        assert_eq!(error.to_string(), "group `pishoo` not found");
    }

    #[test]
    fn default_group_includes_primary_gid_users() {
        let pishoo = first_pishoo("pishoo { }");
        let directory = FakeAccountDirectory::default()
            .with_group("pishoo", 42, &["alice"])
            .with_primary_users(42, &["bob", "carol"]);

        let workers = resolve_all_workers_with_directory(&pishoo, &directory)
            .expect("default group should resolve");

        let usernames = workers
            .iter()
            .map(|worker| worker.username.as_str())
            .collect::<Vec<_>>();
        assert_eq!(usernames, ["alice", "bob", "carol"]);
    }

    #[test]
    fn default_group_deduplicates_supplementary_and_primary_users() {
        let pishoo = first_pishoo("pishoo { }");
        let directory = FakeAccountDirectory::default()
            .with_group("pishoo", 42, &["alice", "bob"])
            .with_primary_users(42, &["bob", "carol"]);

        let workers = resolve_all_workers_with_directory(&pishoo, &directory)
            .expect("default group should resolve");

        let usernames = workers
            .iter()
            .map(|worker| worker.username.as_str())
            .collect::<Vec<_>>();
        assert_eq!(usernames, ["alice", "bob", "carol"]);
    }

    #[test]
    fn explicit_workers_are_kept_before_default_group_users() {
        let pishoo = first_pishoo("pishoo { workers zoe alice; }");
        let directory = FakeAccountDirectory::default().with_group("pishoo", 42, &["bob"]);

        let workers = resolve_all_workers_with_directory(&pishoo, &directory)
            .expect("explicit workers should resolve without default group lookup");

        let usernames = workers
            .iter()
            .map(|worker| worker.username.as_str())
            .collect::<Vec<_>>();
        assert_eq!(usernames, ["zoe", "alice"]);
    }
}
