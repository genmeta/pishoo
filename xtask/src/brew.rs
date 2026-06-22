use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use flate2::{Compression, write::GzEncoder};
use serde::Serialize;
use snafu::{ResultExt, Whatever};
use tracing::{Instrument, info, info_span};

use crate::{
    BrewTarget, Feature, package_meta,
    release_contract::{PackageKind, ReleaseContract, resolve_build_env_from_process},
    run_cmd, run_cmd_quiet, sha256_file, target_dir,
};

const CARGO_NAME: &str = "pishoo";

const BREW_FEATURES_FILE: &str = "pishoo-brew-features.toml";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrewArchive {
    pub target: String,
    pub archive_name: String,
    pub path: PathBuf,
    pub features: Vec<String>,
}

async fn check_cargo() -> Result<(), Whatever> {
    run_cmd_quiet(tokio::process::Command::new("which").arg("cargo")).await
}

/// Create a tar.gz archive from a staging directory.
fn create_tar_gz(staging: &Path, output: &Path) -> Result<(), Whatever> {
    let file = std::fs::File::create(output)
        .whatever_context(format!("failed to create {}", output.display()))?;
    let encoder = GzEncoder::new(file, Compression::default());
    let mut archive = tar::Builder::new(encoder);
    archive
        .append_dir_all(".", staging)
        .whatever_context("failed to append files to tar archive")?;
    archive
        .finish()
        .whatever_context("failed to finalize tar archive")?;
    Ok(())
}

#[derive(Debug, Clone, Copy, Serialize)]
struct BrewFeatures {
    sshd: bool,
    pam: bool,
}

pub async fn run(
    contract: &ReleaseContract,
    targets: &[BrewTarget],
    features: &[Feature],
) -> Result<Vec<BrewArchive>, Whatever> {
    info!(target_count = targets.len(), "starting brew dist build");
    let meta = package_meta(CARGO_NAME)?;
    let target_dir = target_dir()?;
    let workspace = std::env::current_dir().whatever_context("failed to get cwd")?;

    let mut tasks = tokio::task::JoinSet::new();
    for &target in targets {
        let version = meta.version.clone();
        let target_dir = target_dir.clone();
        let workspace = workspace.clone();
        let triple = target.triple();
        let features = features.to_vec();
        let build_env = resolve_build_env_from_process(contract, PackageKind::Brew, Some(triple))
            .whatever_context("failed to resolve build environment for brew target")?;
        info!(triple, "queued brew target build");
        let span = info_span!("brew", triple);
        tasks.spawn(
            async move {
                build_one(
                    triple,
                    &version,
                    &target_dir,
                    &workspace,
                    &features,
                    build_env,
                )
                .await
            }
            .instrument(span),
        );
    }

    info!("waiting for brew target builds to finish");
    let mut archives = Vec::new();
    while let Some(result) = tasks.join_next().await {
        archives.push(result.whatever_context("brew build task panicked")??);
    }
    archives.sort_by(|left, right| left.target.cmp(&right.target));
    info!("finished brew dist build");

    Ok(archives)
}

async fn build_one(
    triple: &str,
    version: &str,
    target_dir: &Path,
    workspace: &Path,
    features: &[Feature],
    build_env: BTreeMap<String, String>,
) -> Result<BrewArchive, Whatever> {
    let has_sshd = features
        .iter()
        .any(|f| matches!(f, Feature::Sshd | Feature::Pam));
    let feature_sidecar = BrewFeatures {
        sshd: has_sshd,
        pam: features.iter().any(|f| matches!(f, Feature::Pam)),
    };

    info!(triple, "checking cargo availability");
    check_cargo().await?;

    // Build cargo features string
    let cargo_features = {
        let names: Vec<&str> = features
            .iter()
            .map(|f| match f {
                Feature::Sshd => "sshd",
                Feature::Pam => "pam",
            })
            .collect();
        if names.is_empty() {
            String::new()
        } else {
            names.join(",")
        }
    };

    info!(triple, "starting cargo build for brew target");
    let mut args = vec!["build", "--release", "--target", triple, "-p", CARGO_NAME];
    if !cargo_features.is_empty() {
        args.push("--features");
        args.push(&cargo_features);
    }
    run_cmd(
        tokio::process::Command::new("cargo")
            .envs(&build_env)
            .args(&args),
    )
    .await
    .whatever_context(format!("cargo build failed for {triple}"))?;
    info!(triple, "cargo build finished for brew target");

    // Stage
    let brew_dir = target_dir.join(triple).join("release").join("brew");
    let staging = brew_dir.join("staging");
    let _ = tokio::fs::remove_dir_all(&staging).await;
    tokio::fs::create_dir_all(&staging)
        .await
        .whatever_context(format!("failed to create {}", staging.display()))?;

    // Copy binaries: pishoo and pishoo-worker are always required
    let release_dir = target_dir.join(triple).join("release");
    tokio::fs::copy(release_dir.join("pishoo"), staging.join("pishoo"))
        .await
        .whatever_context("failed to copy pishoo")?;
    tokio::fs::copy(
        release_dir.join("pishoo-worker"),
        staging.join("pishoo-worker"),
    )
    .await
    .whatever_context("failed to copy pishoo-worker")?;

    // pishoo-ssh-session is only built when sshd feature is enabled
    if has_sshd {
        tokio::fs::copy(
            release_dir.join("pishoo-ssh-session"),
            staging.join("pishoo-ssh-session"),
        )
        .await
        .whatever_context("failed to copy pishoo-ssh-session")?;
    } else {
        info!(
            triple,
            "skipping pishoo-ssh-session (sshd feature not enabled)"
        );
    }

    // Copy config files
    let conf_src = workspace.join("xtask/deb/common/etc/pishoo");
    tokio::fs::copy(conf_src.join("pishoo.conf"), staging.join("pishoo.conf"))
        .await
        .whatever_context("failed to copy pishoo.conf")?;
    tokio::fs::copy(conf_src.join("mime.types"), staging.join("mime.types"))
        .await
        .whatever_context("failed to copy mime.types")?;

    // Rewrite /etc paths to relative etc for Homebrew
    let conf_content = tokio::fs::read_to_string(staging.join("pishoo.conf"))
        .await
        .whatever_context("failed to read staged pishoo.conf")?;
    tokio::fs::write(
        staging.join("pishoo.conf"),
        conf_content.replace("/etc", "etc"),
    )
    .await
    .whatever_context("failed to rewrite pishoo.conf")?;

    // Create tar.gz
    let archive_name = format!("{CARGO_NAME}_{version}-{triple}.tar.gz");
    let archive_path = brew_dir.join(&archive_name);
    {
        let staging = staging.clone();
        let archive_path = archive_path.clone();
        tokio::task::spawn_blocking(move || create_tar_gz(&staging, &archive_path))
            .await
            .whatever_context("tar task panicked")??;
    }

    // Cleanup staging
    let _ = tokio::fs::remove_dir_all(&staging).await;

    // Hash
    let sha = sha256_file(&archive_path).await?;
    write_feature_sidecar(&brew_dir, feature_sidecar).await?;

    info!(path = %archive_path.display(), sha256 = %sha, "produced archive");
    Ok(BrewArchive {
        target: triple.to_string(),
        archive_name,
        path: archive_path,
        features: feature_names(features),
    })
}

pub(crate) fn feature_names(features: &[Feature]) -> Vec<String> {
    features
        .iter()
        .map(|feature| match feature {
            Feature::Sshd => "sshd",
            Feature::Pam => "pam",
        })
        .map(ToOwned::to_owned)
        .collect()
}

async fn write_feature_sidecar(directory: &Path, features: BrewFeatures) -> Result<(), Whatever> {
    let path = directory.join(BREW_FEATURES_FILE);
    let content =
        toml::to_string(&features).whatever_context("failed to serialize brew feature sidecar")?;
    tokio::fs::write(&path, content)
        .await
        .whatever_context(format!("failed to write {}", path.display()))
}
