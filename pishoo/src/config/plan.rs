use dhttp::home::{
    DhttpHome,
    identity::{IdentityProfile, ssl::IdentityProfileCandidateError},
};
use gateway::parse::{
    TypedConfigParser,
    build::BuildTypedConfigError,
    config::{PishooConfig, RootWorkerDefaultsSnapshot, ServerConfig},
    error::ConfigLoadFailure,
};
use snafu::{ResultExt, Snafu};

use super::{
    ConfigError, PishooConfigSource,
    account::{
        BuildWorkerRosterError, WorkerAccount, WorkerAccountError, WorkerRoster,
        select_worker_dhttp_home,
    },
    worker_target::{WorkerDiscoveryMode, resolve_worker_targets},
};

#[derive(Debug, Snafu)]
#[snafu(module(load_global_pishoo_plan_error))]
pub enum LoadGlobalPishooPlanError {
    #[snafu(display("failed to load root configuration"))]
    Config { source: ConfigLoadFailure },
    #[snafu(display("failed to resolve worker directives"))]
    WorkerDirectives { source: ConfigError },
    #[snafu(display("failed to resolve worker accounts"))]
    WorkerAccounts { source: ConfigError },
    #[snafu(display("failed to construct worker account"))]
    WorkerAccount { source: WorkerAccountError },
    #[snafu(display("failed to construct worker roster"))]
    WorkerRoster { source: BuildWorkerRosterError },
}

#[derive(Debug)]
pub struct GlobalPishooPlan {
    source: PishooConfigSource,
    pishoo: PishooConfig,
    worker_defaults: RootWorkerDefaultsSnapshot,
    desired_workers: WorkerRoster,
    direct_servers: Box<[gateway::parse::ServerConfigCandidate]>,
}
impl GlobalPishooPlan {
    pub fn source(&self) -> &PishooConfigSource {
        &self.source
    }
    pub fn home(&self) -> Option<&DhttpHome> {
        self.source.dhttp_home()
    }
    pub fn pishoo(&self) -> &PishooConfig {
        &self.pishoo
    }
    pub fn worker_defaults(&self) -> &RootWorkerDefaultsSnapshot {
        &self.worker_defaults
    }
    pub fn desired_workers(&self) -> &WorkerRoster {
        &self.desired_workers
    }
    pub fn direct_servers(&self) -> &[gateway::parse::ServerConfigCandidate] {
        &self.direct_servers
    }
    pub fn into_parts(
        self,
    ) -> (
        Option<DhttpHome>,
        RootWorkerDefaultsSnapshot,
        WorkerRoster,
        Box<[gateway::parse::ServerConfigCandidate]>,
    ) {
        (
            self.source.dhttp_home().cloned(),
            self.worker_defaults,
            self.desired_workers,
            self.direct_servers,
        )
    }
}

pub async fn load_global_pishoo_plan(
    source: &PishooConfigSource,
) -> Result<GlobalPishooPlan, LoadGlobalPishooPlanError> {
    let mut parser = TypedConfigParser::new();
    let parsed = gateway::parse::load_root_config_file(
        &mut parser,
        source.config_path(),
        source.dhttp_home(),
    )
    .await
    .context(load_global_pishoo_plan_error::ConfigSnafu)?;
    let (pishoo, direct_servers) = parsed.into_parts();
    let mode = if source.default_worker_groups_enabled() {
        WorkerDiscoveryMode::DefaultGlobalHome
    } else {
        WorkerDiscoveryMode::ExplicitConfig
    };
    let workers = resolve_all_workers_with_mode(&pishoo, mode)
        .context(load_global_pishoo_plan_error::WorkerDirectivesSnafu)?;
    let targets = resolve_worker_targets(&workers)
        .context(load_global_pishoo_plan_error::WorkerAccountsSnafu)?;
    let accounts = targets
        .iter()
        .map(|target| {
            WorkerAccount::from_target(target, select_worker_dhttp_home(target))
                .context(load_global_pishoo_plan_error::WorkerAccountSnafu)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let desired_workers =
        WorkerRoster::new(accounts).context(load_global_pishoo_plan_error::WorkerRosterSnafu)?;
    let worker_defaults = pishoo.worker_defaults();
    Ok(GlobalPishooPlan {
        source: source.clone(),
        pishoo,
        worker_defaults,
        desired_workers,
        direct_servers,
    })
}

fn resolve_all_workers_with_mode(
    config: &PishooConfig,
    mode: WorkerDiscoveryMode,
) -> Result<Vec<super::WorkerTarget>, ConfigError> {
    super::worker_target::resolve_all_workers_mode(config, mode)
}

#[derive(Debug, Snafu)]
#[snafu(module(identity_server_error))]
pub enum IdentityServerError {
    #[snafu(display("invalid identity profile candidate"))]
    Profile {
        source: IdentityProfileCandidateError,
    },
    #[snafu(display("failed to load identity server configuration"))]
    Config { source: ConfigLoadFailure },
    #[snafu(display("identity server configuration is invalid"))]
    Server { source: BuildTypedConfigError },
}

#[derive(Debug)]
pub struct IdentityServerCandidate {
    profile: Option<IdentityProfile>,
    result: Result<Option<ServerConfig>, IdentityServerError>,
}
impl IdentityServerCandidate {
    pub fn profile(&self) -> Option<&IdentityProfile> {
        self.profile.as_ref()
    }
    pub fn result(&self) -> &Result<Option<ServerConfig>, IdentityServerError> {
        &self.result
    }
    pub fn into_parts(
        self,
    ) -> (
        Option<IdentityProfile>,
        Result<Option<ServerConfig>, IdentityServerError>,
    ) {
        (self.profile, self.result)
    }
}

#[derive(Debug, Snafu)]
#[snafu(module(load_identity_server_candidates_error))]
pub enum LoadIdentityServerCandidatesError {
    #[snafu(display("failed to enumerate identity profiles"))]
    Enumerate {
        source: dhttp::home::identity::ssl::ListIdentityProfilesError,
    },
}

pub async fn load_identity_server_candidates(
    home: &DhttpHome,
    defaults: &RootWorkerDefaultsSnapshot,
) -> Result<Box<[IdentityServerCandidate]>, LoadIdentityServerCandidatesError> {
    let profiles = home
        .identity_profile_candidates()
        .await
        .context(load_identity_server_candidates_error::EnumerateSnafu)?;
    let mut parser = TypedConfigParser::new();
    let mut results = Vec::with_capacity(profiles.len());
    for profile in profiles.into_vec() {
        match profile {
            Err(source) => results.push(IdentityServerCandidate {
                profile: None,
                result: Err(IdentityServerError::Profile { source }),
            }),
            Ok(profile) => {
                let loaded = gateway::parse::load_identity_config_file(
                    &mut parser,
                    profile.clone(),
                    defaults,
                )
                .await;
                let result = match loaded {
                    Ok(None) => Ok(None),
                    Ok(Some(candidate)) => candidate
                        .into_parts()
                        .1
                        .map(Some)
                        .map_err(|source| IdentityServerError::Server { source }),
                    Err(source) => Err(IdentityServerError::Config { source }),
                };
                results.push(IdentityServerCandidate {
                    profile: Some(profile),
                    result,
                });
            }
        }
    }
    Ok(results.into_boxed_slice())
}

#[derive(Debug)]
pub struct WorkerHomePlan {
    account: WorkerAccount,
    defaults: RootWorkerDefaultsSnapshot,
    servers: Box<[IdentityServerCandidate]>,
}
impl WorkerHomePlan {
    pub fn account(&self) -> &WorkerAccount {
        &self.account
    }
    pub fn defaults(&self) -> &RootWorkerDefaultsSnapshot {
        &self.defaults
    }
    pub fn servers(&self) -> &[IdentityServerCandidate] {
        &self.servers
    }
}

#[derive(Debug, Snafu)]
#[snafu(module(load_worker_home_plan_error))]
pub enum LoadWorkerHomePlanError {
    #[snafu(display("failed to load worker pishoo configuration"))]
    Config { source: ConfigLoadFailure },
    #[snafu(display("failed to load worker identity candidates"))]
    Identities {
        source: LoadIdentityServerCandidatesError,
    },
}

pub async fn load_worker_home_plan(
    account: WorkerAccount,
    root: RootWorkerDefaultsSnapshot,
) -> Result<WorkerHomePlan, LoadWorkerHomePlanError> {
    let home = account.dhttp_home();
    let path = home.join(PishooConfigSource::CONFIG_FILE_NAME);
    let mut parser = TypedConfigParser::new();
    let defaults = match gateway::parse::load_worker_config_file(&mut parser, &path, home, &root)
        .await
        .context(load_worker_home_plan_error::ConfigSnafu)?
    {
        Some(parsed) => parsed.pishoo().worker_defaults(),
        None => root,
    };
    let servers = load_identity_server_candidates(home, &defaults)
        .await
        .context(load_worker_home_plan_error::IdentitiesSnafu)?;
    Ok(WorkerHomePlan {
        account,
        defaults,
        servers,
    })
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use dhttp::home::DhttpHome;

    use super::*;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "pishoo-{label}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn snapshot_and_roster_come_from_one_valid_root_plan() {
        let temp = TempDir::new("root-plan");
        let path = temp.path().join("pishoo.conf");
        std::fs::write(&path, "pishoo { gzip on; }").unwrap();
        let source = PishooConfigSource::explicit_at(path, temp.path()).unwrap();

        let plan = load_global_pishoo_plan(&source).await.unwrap();

        assert!(plan.worker_defaults().http().gzip().effective().0);
        assert!(plan.desired_workers().is_empty());
    }

    #[tokio::test]
    async fn bad_identity_server_does_not_fail_sibling_candidate() {
        let temp = TempDir::new("identity-siblings");
        for (name, server) in [
            ("good.dhttp.net", "server { listen all 443; }"),
            ("bad.dhttp.net", "server {"),
        ] {
            let profile = temp.path().join(name);
            std::fs::create_dir_all(profile.join("ssl")).unwrap();
            std::fs::write(profile.join("server.conf"), server).unwrap();
        }
        let home = DhttpHome::new(temp.path().to_path_buf());
        let defaults = gateway::parse::TypedConfigParser::new()
            .parse_root("pishoo {}", &temp.path().join("root.conf"), Some(&home))
            .unwrap()
            .pishoo()
            .worker_defaults();

        let children = load_identity_server_candidates(&home, &defaults)
            .await
            .unwrap();

        assert_eq!(
            children
                .iter()
                .filter(|child| child.result().is_ok())
                .count(),
            1
        );
        assert_eq!(
            children
                .iter()
                .filter(|child| child.result().is_err())
                .count(),
            1
        );
    }
}
