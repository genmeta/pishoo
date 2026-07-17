use gateway::parse::config::PishooConfig;
pub use nix::unistd::{Gid, Uid, User as ResolvedWorkerTarget};
use snafu::{OptionExt, ResultExt};

use super::{
    ConfigError, EmptyWorkerNameSnafu, GroupNotFoundSnafu, GroupResolveSnafu, MissingHomeSnafu,
    PrimaryGroupUserResolveSnafu, UserNotFoundSnafu, UserResolveSnafu,
};

#[derive(Debug, Clone)]
pub struct WorkerTarget {
    pub username: String,
}

#[cfg(target_os = "macos")]
const DEFAULT_GROUPS: &[&str] = &["_www"];

#[cfg(not(target_os = "macos"))]
const DEFAULT_GROUPS: &[&str] = &["dhttp"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerDiscoveryMode {
    DefaultGlobalHome,
    ExplicitConfig,
}

impl WorkerDiscoveryMode {
    pub fn default_groups_enabled(self) -> bool {
        matches!(self, Self::DefaultGlobalHome)
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

fn parse_configured_workers(pishoo: &PishooConfig) -> Result<Vec<WorkerTarget>, ConfigError> {
    pishoo
        .workers()
        .map_or(Ok(Vec::new()), |names| parse_worker_names(&names.0))
}

fn parse_groups(pishoo: &PishooConfig) -> Vec<String> {
    pishoo
        .groups()
        .map(|names| names.0.clone())
        .unwrap_or_default()
}

#[derive(Debug, Clone)]
pub(super) struct AccountGroup {
    pub(super) name: String,
    pub(super) gid: Gid,
    pub(super) members: Vec<String>,
}

pub(super) trait AccountDirectory {
    fn group_by_name(&self, group_name: &str) -> Result<Option<AccountGroup>, ConfigError>;

    fn group_member_usernames(&self, group_name: &str) -> Result<Option<Vec<String>>, ConfigError> {
        let Some(group) = self.group_by_name(group_name)? else {
            return Ok(None);
        };

        let mut names = group.members;
        names.extend(self.primary_group_usernames(&group.name, group.gid)?);
        Ok(Some(names))
    }

    fn primary_group_usernames(
        &self,
        _group_name: &str,
        _gid: Gid,
    ) -> Result<Vec<String>, ConfigError> {
        Ok(Vec::new())
    }
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

    #[cfg(target_os = "macos")]
    fn group_member_usernames(&self, group_name: &str) -> Result<Option<Vec<String>>, ConfigError> {
        let Some(group) = self.group_by_name(group_name)? else {
            return Ok(None);
        };

        Ok(Some(
            enumerate_passwd_users(&group.name)?
                .into_iter()
                .filter_map(|user| {
                    match macos_membership::user_is_member_of_group(user.uid, group.gid) {
                        Ok(true) => Some(Ok(user.name)),
                        Ok(false) => None,
                        Err(error) => Some(Err(error)),
                    }
                })
                .collect::<Result<Vec<_>, ConfigError>>()?,
        ))
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

#[cfg(target_os = "macos")]
mod macos_membership {
    use nix::{
        errno::Errno,
        unistd::{Gid, Uid},
    };
    use snafu::ResultExt;

    use crate::config::{
        ConfigError, MacosGroupUuidSnafu, MacosMembershipCheckSnafu, MacosUserUuidSnafu,
    };

    type MembershipUuid = [libc::c_uchar; 16];

    unsafe extern "C" {
        fn mbr_uid_to_uuid(id: libc::uid_t, uu: *mut libc::c_uchar) -> libc::c_int;
        fn mbr_gid_to_uuid(id: libc::gid_t, uu: *mut libc::c_uchar) -> libc::c_int;
        fn mbr_check_membership(
            user: *const libc::c_uchar,
            group: *const libc::c_uchar,
            ismember: *mut libc::c_int,
        ) -> libc::c_int;
    }

    fn user_uuid(uid: Uid) -> Result<MembershipUuid, ConfigError> {
        let mut uuid: MembershipUuid = [0; 16];
        let rc = unsafe { mbr_uid_to_uuid(uid.as_raw(), uuid.as_mut_ptr()) };
        Errno::result(rc)
            .map(|_| uuid)
            .context(MacosUserUuidSnafu { uid: uid.as_raw() })
    }

    fn group_uuid(gid: Gid) -> Result<MembershipUuid, ConfigError> {
        let mut uuid: MembershipUuid = [0; 16];
        let rc = unsafe { mbr_gid_to_uuid(gid.as_raw(), uuid.as_mut_ptr()) };
        Errno::result(rc)
            .map(|_| uuid)
            .context(MacosGroupUuidSnafu { gid: gid.as_raw() })
    }

    pub(super) fn user_is_member_of_group(uid: Uid, gid: Gid) -> Result<bool, ConfigError> {
        let user_uuid = user_uuid(uid)?;
        let group_uuid = group_uuid(gid)?;
        let mut is_member: libc::c_int = 0;
        let rc = unsafe {
            mbr_check_membership(user_uuid.as_ptr(), group_uuid.as_ptr(), &mut is_member)
        };
        Errno::result(rc)
            .map(|_| is_member != 0)
            .context(MacosMembershipCheckSnafu {
                uid: uid.as_raw(),
                gid: gid.as_raw(),
            })
    }
}

fn resolve_group_members_with_directory<D: AccountDirectory>(
    directory: &D,
    group_name: &str,
) -> Result<Option<Vec<WorkerTarget>>, ConfigError> {
    let Some(names) = directory.group_member_usernames(group_name)? else {
        return Ok(None);
    };

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
                group = group_name,
                "default worker group not found; continuing without default workers"
            ),
        }
    }
    Ok(targets)
}

pub(super) fn resolve_all_workers_mode(
    pishoo: &PishooConfig,
    mode: WorkerDiscoveryMode,
) -> Result<Vec<WorkerTarget>, ConfigError> {
    resolve_all_workers_with_directory(pishoo, &SystemAccountDirectory, mode)
}

pub(super) fn resolve_all_workers_with_directory<D: AccountDirectory>(
    pishoo: &PishooConfig,
    directory: &D,
    mode: WorkerDiscoveryMode,
) -> Result<Vec<WorkerTarget>, ConfigError> {
    let explicit_workers = parse_configured_workers(pishoo)?;
    let groups = parse_groups(pishoo);

    let group_members =
        if groups.is_empty() && explicit_workers.is_empty() && mode.default_groups_enabled() {
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

#[cfg(test)]
mod tests {
    use super::DEFAULT_GROUPS;

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_default_worker_group_is_www() {
        assert_eq!(DEFAULT_GROUPS, &["_www"]);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn non_macos_default_worker_group_is_dhttp() {
        assert_eq!(DEFAULT_GROUPS, &["dhttp"]);
    }
}
