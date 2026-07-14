use std::path::PathBuf;

use snafu::Snafu;

mod account;
pub(crate) mod plan;
pub mod source;
pub mod worker_target;

pub use account::{WorkerAccount, WorkerDiff, WorkerRoster, compute_worker_diff};
pub use plan::{
    GlobalPishooPlan, IdentityServerCandidate, WorkerHomePlan, load_global_pishoo_plan,
    load_identity_server_candidates, load_worker_home_plan,
};
pub use source::{PishooConfigSource, ResolveConfigSourceError};
pub use worker_target::{ResolvedWorkerTarget, WorkerTarget, resolve_worker_targets};

#[derive(Debug, Snafu)]
pub enum ConfigError {
    #[snafu(display("worker username cannot be empty"))]
    EmptyWorkerName,
    #[snafu(display("failed to resolve users in group `{group_name}`"))]
    GroupResolve {
        group_name: String,
        source: nix::Error,
    },
    #[snafu(display("group `{group_name}` not found"))]
    GroupNotFound { group_name: String },
    #[snafu(display("failed to enumerate users with primary group `{group_name}`"))]
    PrimaryGroupUserResolve {
        group_name: String,
        source: nix::errno::Errno,
    },
    #[snafu(display("failed to resolve macOS membership uuid for uid {uid}"))]
    MacosUserUuid { uid: u32, source: nix::errno::Errno },
    #[snafu(display("failed to resolve macOS membership uuid for gid {gid}"))]
    MacosGroupUuid { gid: u32, source: nix::errno::Errno },
    #[snafu(display("failed to check macOS group membership for uid {uid} in gid {gid}"))]
    MacosMembershipCheck {
        uid: u32,
        gid: u32,
        source: nix::errno::Errno,
    },
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
pub fn pid_path(config: &gateway::parse::config::PishooConfig) -> PathBuf {
    config
        .pid()
        .map(|path| path.as_ref().to_path_buf())
        .unwrap_or_else(|| PID_FILE_DEFAULT.into())
}
