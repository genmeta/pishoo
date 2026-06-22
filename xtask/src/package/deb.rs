use std::path::{Path, PathBuf};

use snafu::{ResultExt, Snafu, Whatever};

use super::{
    PackageArtifact, PackageManifest,
    manifest::ArtifactKind,
    prompt::{self, OverwriteDecision},
};
use crate::{BuildProfile, DebTarget, Feature, deb::DebArtifact};

const PACKAGE_NAME: &str = "pishoo";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebMetadata {
    pub package_name: String,
    pub package_version: String,
    pub architecture: String,
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum DebMetadataError {
    #[snafu(display("deb metadata is missing Package"))]
    Package,
    #[snafu(display("deb metadata is missing Version"))]
    Version,
    #[snafu(display("deb metadata is missing Architecture"))]
    Architecture,
}

#[derive(Debug, Snafu)]
#[snafu(module)]
enum ReadDebMetadataError {
    #[snafu(display("failed to run dpkg-deb metadata command"))]
    Spawn {
        source: std::io::Error,
        path: PathBuf,
    },
    #[snafu(display("dpkg-deb metadata command failed"))]
    Command { path: PathBuf },
    #[snafu(display("dpkg-deb metadata output was not utf-8"))]
    Utf8 {
        source: std::string::FromUtf8Error,
        path: PathBuf,
    },
    #[snafu(display("failed to parse deb metadata"))]
    Parse {
        source: DebMetadataError,
        path: PathBuf,
    },
}

#[derive(Debug, Snafu)]
#[snafu(module)]
enum DebPackageManifestError {
    #[snafu(display("failed to make artifact path target-relative"))]
    TargetRelativePath { source: std::path::StripPrefixError },
    #[snafu(display("artifact path must be valid utf-8"))]
    ArtifactPathUtf8,
}

pub fn parse_dpkg_field_output(output: &str) -> Result<DebMetadata, DebMetadataError> {
    let mut package_name = None;
    let mut package_version = None;
    let mut architecture = None;
    for line in output.lines() {
        if let Some(value) = line.strip_prefix("Package:") {
            package_name = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("Version:") {
            package_version = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("Architecture:") {
            architecture = Some(value.trim().to_string());
        }
    }
    Ok(DebMetadata {
        package_name: package_name.ok_or(DebMetadataError::Package)?,
        package_version: package_version.ok_or(DebMetadataError::Version)?,
        architecture: architecture.ok_or(DebMetadataError::Architecture)?,
    })
}

async fn read_deb_metadata(path: &Path) -> Result<DebMetadata, ReadDebMetadataError> {
    let output = tokio::process::Command::new("dpkg-deb")
        .arg("--field")
        .arg(path)
        .output()
        .await
        .context(read_deb_metadata_error::SpawnSnafu {
            path: path.to_path_buf(),
        })?;
    snafu::ensure!(
        output.status.success(),
        read_deb_metadata_error::CommandSnafu {
            path: path.to_path_buf()
        }
    );
    let stdout = String::from_utf8(output.stdout).context(read_deb_metadata_error::Utf8Snafu {
        path: path.to_path_buf(),
    })?;
    parse_dpkg_field_output(&stdout).context(read_deb_metadata_error::ParseSnafu {
        path: path.to_path_buf(),
    })
}

pub async fn run(
    contract: &crate::release_contract::ReleaseContract,
    targets: &[DebTarget],
    profile: BuildProfile,
    features: &[Feature],
    siblings: &[PathBuf],
    overwrite_manifest: bool,
) -> Result<(), Whatever> {
    let deb_artifacts = crate::deb::run(contract, targets, profile, features, siblings).await?;
    let meta = crate::package_meta("pishoo")?;
    let target_dir = crate::target_dir()?;
    let manifest_path = target_dir.join("common").join("deb").join("manifest.toml");
    let exists = tokio::fs::try_exists(&manifest_path)
        .await
        .whatever_context(format!("failed to inspect {}", manifest_path.display()))?;
    if prompt::confirm_manifest_overwrite(exists, overwrite_manifest)
        .await
        .whatever_context("failed to confirm deb package manifest overwrite")?
        == OverwriteDecision::Skip
    {
        return Ok(());
    }

    let mut artifacts = Vec::new();
    for deb_artifact in &deb_artifacts {
        artifacts.push(manifest_artifact(deb_artifact, &target_dir, profile).await?);
    }

    let manifest = PackageManifest {
        schema_version: 1,
        kind: ArtifactKind::Deb,
        package: PACKAGE_NAME.to_string(),
        version: meta.version,
        generated_at: generated_at(),
        git_commit: None,
        git_dirty: false,
        artifacts,
    };
    super::manifest::write_manifest(&manifest_path, &manifest)
        .await
        .whatever_context("failed to write deb package manifest")?;
    Ok(())
}

async fn manifest_artifact(
    deb_artifact: &DebArtifact,
    target_dir: &Path,
    _profile: BuildProfile,
) -> Result<PackageArtifact, Whatever> {
    let metadata = read_deb_metadata(&deb_artifact.path)
        .await
        .whatever_context("failed to read deb package metadata")?;
    let path = target_relative_artifact_path(&deb_artifact.path, target_dir)
        .whatever_context("failed to make deb artifact path target-relative")?;
    let sha256 = crate::sha256_file(&deb_artifact.path).await?;
    let size = tokio::fs::metadata(&deb_artifact.path)
        .await
        .whatever_context(format!("failed to inspect {}", deb_artifact.path.display()))?
        .len();
    Ok(PackageArtifact {
        target: deb_artifact.target.clone(),
        path,
        sha256,
        size,
        package_name: Some(metadata.package_name),
        package_version: Some(metadata.package_version),
        architecture: Some(metadata.architecture),
        archive_name: deb_artifact
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .map(ToOwned::to_owned),
        features: deb_artifact.features.clone(),
        profile: Some(deb_artifact.profile.target_dir_name().to_string()),
    })
}

fn target_relative_artifact_path(
    path: &Path,
    target_dir: &Path,
) -> Result<String, DebPackageManifestError> {
    path.strip_prefix(target_dir)
        .context(deb_package_manifest_error::TargetRelativePathSnafu)?
        .to_str()
        .map(ToOwned::to_owned)
        .ok_or(DebPackageManifestError::ArtifactPathUtf8)
}

fn generated_at() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

#[cfg(test)]
mod tests {
    use super::parse_dpkg_field_output;

    #[test]
    fn parses_dpkg_field_output() {
        let metadata =
            parse_dpkg_field_output("Package: gmutils\nVersion: 0.5.2-1\nArchitecture: amd64\n")
                .expect("metadata should parse");

        assert_eq!(metadata.package_name, "gmutils");
        assert_eq!(metadata.package_version, "0.5.2-1");
        assert_eq!(metadata.architecture, "amd64");
    }

    #[test]
    fn missing_architecture_fails() {
        let error = parse_dpkg_field_output("Package: gmutils\nVersion: 0.5.2-1\n")
            .expect_err("missing architecture should fail");
        assert_eq!(error.to_string(), "deb metadata is missing Architecture");
    }
}
