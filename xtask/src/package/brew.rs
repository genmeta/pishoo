use std::{collections::BTreeSet, path::Path};

use snafu::{ResultExt, Snafu, Whatever};

use super::{PackageArtifact, PackageManifest, manifest::ArtifactKind, prompt::OverwriteDecision};
use crate::{BrewTarget, Feature, brew::BrewArchive};

const PACKAGE_NAME: &str = "pishoo";
const SUPPORTED_BREW_TARGETS: &[&str] = &["aarch64-apple-darwin", "x86_64-apple-darwin"];

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum BrewPackageError {
    #[snafu(display("brew package must include all supported targets"))]
    MissingSupportedTarget,
    #[snafu(display("brew archive name must include package version"))]
    ArchiveVersion,
    #[snafu(display("failed to make artifact path target-relative"))]
    TargetRelativePath { source: std::path::StripPrefixError },
    #[snafu(display("artifact path must be valid utf-8"))]
    ArtifactPathUtf8,
}

pub fn validate_complete_brew_targets(archives: &[BrewArchive]) -> Result<(), BrewPackageError> {
    let actual = archives
        .iter()
        .map(|archive| archive.target.as_str())
        .collect::<BTreeSet<_>>();
    let expected = SUPPORTED_BREW_TARGETS
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    snafu::ensure!(
        actual == expected,
        brew_package_error::MissingSupportedTargetSnafu
    );
    Ok(())
}

pub fn brew_manifest_artifacts(
    archives: &[BrewArchive],
    version: &str,
) -> Result<Vec<PackageArtifact>, BrewPackageError> {
    validate_complete_brew_targets(archives)?;
    archives
        .iter()
        .map(|archive| {
            snafu::ensure!(
                archive.archive_name.contains(version),
                brew_package_error::ArchiveVersionSnafu
            );
            Ok(PackageArtifact {
                target: archive.target.clone(),
                path: target_relative_path(&archive.path)?,
                sha256: String::new(),
                size: 0,
                package_name: None,
                package_version: None,
                architecture: None,
                archive_name: Some(archive.archive_name.clone()),
                features: archive.features.clone(),
                profile: Some("release".to_string()),
            })
        })
        .collect()
}

fn target_relative_path(path: &Path) -> Result<String, BrewPackageError> {
    let target_relative = match path.strip_prefix("target") {
        Ok(relative) => relative,
        Err(_) => path,
    };
    target_relative
        .to_str()
        .map(ToOwned::to_owned)
        .ok_or(BrewPackageError::ArtifactPathUtf8)
}

pub async fn run(
    targets: &[BrewTarget],
    features: &[Feature],
    overwrite_manifest: bool,
) -> Result<(), Whatever> {
    let archives = crate::brew::run(targets, features).await?;
    let meta = crate::package_meta("pishoo")?;
    let target_dir = crate::target_dir()?;
    let manifest_path = target_dir.join("common").join("brew").join("manifest.toml");
    let exists = tokio::fs::try_exists(&manifest_path)
        .await
        .whatever_context(format!("failed to inspect {}", manifest_path.display()))?;
    if super::prompt::confirm_manifest_overwrite(exists, overwrite_manifest)
        .await
        .whatever_context("failed to confirm brew package manifest overwrite")?
        == OverwriteDecision::Skip
    {
        return Ok(());
    }

    let mut artifacts = brew_manifest_artifacts(&archives, &meta.version)
        .whatever_context("failed to build brew package manifest artifacts")?;
    for (artifact, archive) in artifacts.iter_mut().zip(archives.iter()) {
        artifact.path = target_relative_artifact_path(&archive.path, &target_dir)
            .whatever_context("failed to make brew artifact path target-relative")?;
        artifact.sha256 = crate::sha256_file(&archive.path).await?;
        artifact.size = tokio::fs::metadata(&archive.path)
            .await
            .whatever_context(format!("failed to inspect {}", archive.path.display()))?
            .len();
    }

    let manifest = PackageManifest {
        schema_version: 1,
        kind: ArtifactKind::Brew,
        package: PACKAGE_NAME.to_string(),
        version: meta.version,
        generated_at: generated_at(),
        git_commit: None,
        git_dirty: false,
        artifacts,
    };
    super::manifest::write_manifest(&manifest_path, &manifest)
        .await
        .whatever_context("failed to write brew package manifest")?;
    Ok(())
}

fn target_relative_artifact_path(
    path: &Path,
    target_dir: &Path,
) -> Result<String, BrewPackageError> {
    path.strip_prefix(target_dir)
        .context(brew_package_error::TargetRelativePathSnafu)?
        .to_str()
        .map(ToOwned::to_owned)
        .ok_or(BrewPackageError::ArtifactPathUtf8)
}

fn generated_at() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{brew_manifest_artifacts, validate_complete_brew_targets};
    use crate::brew::BrewArchive;

    #[test]
    fn brew_requires_all_supported_targets() {
        let archives = vec![BrewArchive {
            target: "aarch64-apple-darwin".to_string(),
            archive_name: "gmutils-0.5.2-aarch64-apple-darwin.tar.gz".to_string(),
            path: PathBuf::from(
                "target/aarch64-apple-darwin/release/brew/gmutils-0.5.2-aarch64-apple-darwin.tar.gz",
            ),
            features: Vec::new(),
        }];

        let error =
            validate_complete_brew_targets(&archives).expect_err("missing x86_64 should fail");
        assert_eq!(
            error.to_string(),
            "brew package must include all supported targets"
        );
    }

    #[test]
    fn brew_archive_name_must_include_version() {
        let archives = vec![
            BrewArchive {
                target: "aarch64-apple-darwin".to_string(),
                archive_name: "gmutils-aarch64-apple-darwin.tar.gz".to_string(),
                path: PathBuf::from(
                    "target/aarch64-apple-darwin/release/brew/gmutils-aarch64-apple-darwin.tar.gz",
                ),
                features: Vec::new(),
            },
            BrewArchive {
                target: "x86_64-apple-darwin".to_string(),
                archive_name: "gmutils-0.5.2-x86_64-apple-darwin.tar.gz".to_string(),
                path: PathBuf::from(
                    "target/x86_64-apple-darwin/release/brew/gmutils-0.5.2-x86_64-apple-darwin.tar.gz",
                ),
                features: Vec::new(),
            },
        ];

        let error =
            brew_manifest_artifacts(&archives, "0.5.2").expect_err("missing version should fail");
        assert_eq!(
            error.to_string(),
            "brew archive name must include package version"
        );
    }

    #[test]
    fn brew_manifest_records_features() {
        let artifact = build_brew_artifact_for_test(
            "aarch64-apple-darwin",
            "pishoo_0.5.2-aarch64-apple-darwin.tar.gz",
            &["sshd", "pam"],
        );
        assert_eq!(artifact.features, ["sshd", "pam"]);
    }

    fn build_brew_artifact_for_test(
        target: &str,
        archive_name: &str,
        features: &[&str],
    ) -> crate::package::PackageArtifact {
        crate::package::PackageArtifact {
            target: target.to_string(),
            path: format!("{target}/release/brew/{archive_name}"),
            sha256: "0".repeat(64),
            size: 1,
            package_name: None,
            package_version: None,
            architecture: None,
            archive_name: Some(archive_name.to_string()),
            features: features.iter().map(|feature| feature.to_string()).collect(),
            profile: Some("release".to_string()),
        }
    }
}
