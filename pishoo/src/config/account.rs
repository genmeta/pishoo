use std::{collections::BTreeMap, path::PathBuf};

use dhttp::home::DhttpHome;
use nix::unistd::{Gid, Uid};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use snafu::{Snafu, ensure};

use super::ResolvedWorkerTarget;

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum WorkerAccountError {
    #[snafu(display("worker account name cannot be empty"))]
    EmptyName,
    #[snafu(display("worker login home must be absolute: {}", path.display()))]
    RelativeLoginHome { path: PathBuf },
    #[snafu(display("worker dhttp home must be absolute: {}", path.display()))]
    RelativeDhttpHome { path: PathBuf },
}

#[derive(Clone, Debug)]
pub(crate) struct WorkerAccount {
    name: String,
    uid: Uid,
    primary_gid: Gid,
    login_home: PathBuf,
    dhttp_home: DhttpHome,
}

impl WorkerAccount {
    pub(crate) fn new(
        name: String,
        uid: Uid,
        primary_gid: Gid,
        login_home: PathBuf,
        dhttp_home: DhttpHome,
    ) -> Result<Self, WorkerAccountError> {
        ensure!(!name.is_empty(), worker_account_error::EmptyNameSnafu);
        ensure!(
            login_home.is_absolute(),
            worker_account_error::RelativeLoginHomeSnafu { path: login_home }
        );
        ensure!(
            dhttp_home.as_path().is_absolute(),
            worker_account_error::RelativeDhttpHomeSnafu {
                path: dhttp_home.as_path().to_path_buf()
            }
        );
        Ok(Self {
            name,
            uid,
            primary_gid,
            login_home,
            dhttp_home,
        })
    }

    pub(crate) fn from_target(
        target: &ResolvedWorkerTarget,
        dhttp_home: DhttpHome,
    ) -> Result<Self, WorkerAccountError> {
        Self::new(
            target.name.clone(),
            target.uid,
            target.gid,
            target.dir.clone(),
            dhttp_home,
        )
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) const fn uid(&self) -> Uid {
        self.uid
    }

    pub(crate) const fn primary_gid(&self) -> Gid {
        self.primary_gid
    }

    pub(crate) fn login_home(&self) -> &std::path::Path {
        &self.login_home
    }

    pub(crate) fn dhttp_home(&self) -> &DhttpHome {
        &self.dhttp_home
    }
}

impl PartialEq for WorkerAccount {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.uid == other.uid
            && self.primary_gid == other.primary_gid
            && self.login_home == other.login_home
            && self.dhttp_home.as_path() == other.dhttp_home.as_path()
    }
}

impl Eq for WorkerAccount {}

#[derive(Serialize, Deserialize)]
struct WorkerAccountWire {
    name: String,
    uid: u32,
    primary_gid: u32,
    login_home: PathBuf,
    dhttp_home: PathBuf,
}

impl Serialize for WorkerAccount {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        WorkerAccountWire {
            name: self.name.clone(),
            uid: self.uid.as_raw(),
            primary_gid: self.primary_gid.as_raw(),
            login_home: self.login_home.clone(),
            dhttp_home: self.dhttp_home.as_path().to_path_buf(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for WorkerAccount {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = WorkerAccountWire::deserialize(deserializer)?;
        Self::new(
            wire.name,
            Uid::from_raw(wire.uid),
            Gid::from_raw(wire.primary_gid),
            wire.login_home,
            DhttpHome::new(wire.dhttp_home),
        )
        .map_err(D::Error::custom)
    }
}

pub(crate) fn select_worker_dhttp_home(target: &ResolvedWorkerTarget) -> DhttpHome {
    DhttpHome::for_user_home_dir(target.dir.clone())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WorkerCredentialSnapshot {
    pub(crate) real_uid: Uid,
    pub(crate) effective_uid: Uid,
    pub(crate) real_gid: Gid,
    pub(crate) effective_gid: Gid,
}

#[derive(Debug, Snafu)]
pub(crate) enum BuildWorkerRosterError {
    #[snafu(display("duplicate worker uid {uid}"))]
    DuplicateUid { uid: Uid },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkerRoster(BTreeMap<u32, WorkerAccount>);

impl WorkerRoster {
    pub(crate) fn new(
        accounts: impl IntoIterator<Item = WorkerAccount>,
    ) -> Result<Self, BuildWorkerRosterError> {
        let mut roster = BTreeMap::new();
        for account in accounts {
            let uid = account.uid().as_raw();
            if roster.insert(uid, account).is_some() {
                return Err(BuildWorkerRosterError::DuplicateUid {
                    uid: Uid::from_raw(uid),
                });
            }
        }
        Ok(Self(roster))
    }

    pub(crate) fn get(&self, uid: Uid) -> Option<&WorkerAccount> {
        self.0.get(&uid.as_raw())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use dhttp::home::DhttpHome;
    use nix::unistd::{Gid, Uid, User};

    use super::{
        BuildWorkerRosterError, WorkerAccount, WorkerAccountError, WorkerCredentialSnapshot,
        WorkerRoster, select_worker_dhttp_home,
    };

    fn account() -> WorkerAccount {
        WorkerAccount::new(
            "alice".to_owned(),
            Uid::from_raw(1000),
            Gid::from_raw(100),
            PathBuf::from("/home/alice"),
            DhttpHome::new(PathBuf::from("/srv/dhttp/alice")),
        )
        .unwrap()
    }

    #[test]
    fn worker_account_rejects_empty_name() {
        let error = WorkerAccount::new(
            String::new(),
            Uid::from_raw(1),
            Gid::from_raw(2),
            PathBuf::from("/home/alice"),
            DhttpHome::new(PathBuf::from("/srv/dhttp/alice")),
        )
        .unwrap_err();
        assert!(matches!(error, WorkerAccountError::EmptyName));
    }

    #[test]
    fn worker_account_rejects_relative_dhttp_home() {
        let error = WorkerAccount::new(
            "alice".to_owned(),
            Uid::from_raw(1),
            Gid::from_raw(2),
            PathBuf::from("/home/alice"),
            DhttpHome::new(PathBuf::from("relative/.dhttp")),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            WorkerAccountError::RelativeDhttpHome { .. }
        ));
    }

    #[test]
    fn worker_account_rejects_relative_login_home() {
        let error = WorkerAccount::new(
            "alice".to_owned(),
            Uid::from_raw(1),
            Gid::from_raw(2),
            PathBuf::from("relative"),
            DhttpHome::new(PathBuf::from("/srv/dhttp/alice")),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            WorkerAccountError::RelativeLoginHome { .. }
        ));
    }

    #[test]
    fn worker_account_keeps_login_home_distinct_from_dhttp_home() {
        let account = account();
        assert_eq!(account.login_home(), std::path::Path::new("/home/alice"));
        assert_eq!(
            account.dhttp_home().as_path(),
            std::path::Path::new("/srv/dhttp/alice")
        );
    }

    #[test]
    fn worker_account_full_equality_includes_gid_and_both_homes() {
        let original = account();
        let different_gid = WorkerAccount::new(
            original.name().to_owned(),
            original.uid(),
            Gid::from_raw(101),
            original.login_home().to_path_buf(),
            original.dhttp_home().clone(),
        )
        .unwrap();
        let different_login = WorkerAccount::new(
            original.name().to_owned(),
            original.uid(),
            original.primary_gid(),
            PathBuf::from("/different/login"),
            original.dhttp_home().clone(),
        )
        .unwrap();
        let different_dhttp = WorkerAccount::new(
            original.name().to_owned(),
            original.uid(),
            original.primary_gid(),
            original.login_home().to_path_buf(),
            DhttpHome::new(PathBuf::from("/different/dhttp")),
        )
        .unwrap();
        assert_ne!(original, different_gid);
        assert_ne!(original, different_login);
        assert_ne!(original, different_dhttp);
        assert_eq!(original, original.clone());
    }

    #[test]
    fn worker_roster_rejects_duplicate_uid() {
        let original = account();
        let duplicate = WorkerAccount::new(
            "bob".to_owned(),
            original.uid(),
            Gid::from_raw(999),
            PathBuf::from("/home/bob"),
            DhttpHome::new(PathBuf::from("/home/bob/.dhttp")),
        )
        .unwrap();
        let error = WorkerRoster::new([original, duplicate]).unwrap_err();
        assert!(matches!(error, BuildWorkerRosterError::DuplicateUid { .. }));
    }

    #[test]
    fn worker_wire_preserves_root_selected_dhttp_home_distinct_from_login_home() {
        let original = account();
        let encoded = serde_json::to_vec(&original).unwrap();
        let decoded: WorkerAccount = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, original);
        assert_ne!(decoded.login_home(), decoded.dhttp_home().as_path());
    }

    #[test]
    fn worker_credentials_keep_real_and_effective_ids_typed() {
        let credentials = WorkerCredentialSnapshot {
            real_uid: Uid::from_raw(1),
            effective_uid: Uid::from_raw(2),
            real_gid: Gid::from_raw(3),
            effective_gid: Gid::from_raw(4),
        };
        assert_eq!(credentials.real_uid, Uid::from_raw(1));
        assert_eq!(credentials.effective_uid, Uid::from_raw(2));
        assert_eq!(credentials.real_gid, Gid::from_raw(3));
        assert_eq!(credentials.effective_gid, Gid::from_raw(4));
    }

    #[test]
    fn default_worker_home_selection_uses_the_login_home_domain_entry_point() {
        let target = User {
            name: "alice".to_owned(),
            passwd: std::ffi::CString::new("x").unwrap(),
            uid: Uid::from_raw(1000),
            gid: Gid::from_raw(100),
            gecos: std::ffi::CString::new("").unwrap(),
            dir: PathBuf::from("/home/alice"),
            shell: PathBuf::from("/bin/sh"),
        };
        assert_eq!(
            select_worker_dhttp_home(&target).as_path(),
            std::path::Path::new("/home/alice/.dhttp")
        );
    }
}
