use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use snafu::{ResultExt, Snafu, Whatever};

use super::{
    PackageArtifact, PackageManifest,
    manifest::ArtifactKind,
    prompt::{self, OverwriteDecision},
};
use crate::{Feature, RpmTarget, rpm::RpmArtifact};

const PACKAGE_NAME: &str = "pishoo";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpmMetadata {
    pub package_name: String,
    pub package_version: String,
    pub architecture: String,
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum RpmMetadataError {
    #[snafu(display("rpm metadata query returned incomplete output"))]
    IncompleteOutput,
}

#[derive(Debug, Snafu)]
#[snafu(module)]
enum ReadRpmMetadataError {
    #[snafu(display("failed to run rpm metadata command"))]
    Spawn {
        source: std::io::Error,
        path: PathBuf,
    },
    #[snafu(display("rpm metadata command failed"))]
    Command { path: PathBuf },
    #[snafu(display("rpm metadata output was not utf-8"))]
    Utf8 {
        source: std::string::FromUtf8Error,
        path: PathBuf,
    },
    #[snafu(display("failed to parse rpm metadata"))]
    Parse {
        source: RpmMetadataError,
        path: PathBuf,
    },
}

#[derive(Debug, Snafu)]
#[snafu(module)]
enum RpmPackageManifestError {
    #[snafu(display("failed to make artifact path target-relative"))]
    TargetRelativePath { source: std::path::StripPrefixError },
    #[snafu(display("artifact path must be valid utf-8"))]
    ArtifactPathUtf8,
}

pub fn parse_rpm_query_output(output: &str) -> Result<RpmMetadata, RpmMetadataError> {
    let mut lines = output.lines();
    let Some(package_name) = lines.next() else {
        return Err(RpmMetadataError::IncompleteOutput);
    };
    let Some(package_version) = lines.next() else {
        return Err(RpmMetadataError::IncompleteOutput);
    };
    let Some(architecture) = lines.next() else {
        return Err(RpmMetadataError::IncompleteOutput);
    };
    Ok(RpmMetadata {
        package_name: package_name.to_string(),
        package_version: package_version.to_string(),
        architecture: architecture.to_string(),
    })
}

async fn read_rpm_metadata(path: &Path) -> Result<RpmMetadata, ReadRpmMetadataError> {
    let output = tokio::process::Command::new("rpm")
        .arg("-qp")
        .arg("--queryformat")
        .arg("%{NAME}\n%{VERSION}-%{RELEASE}\n%{ARCH}\n")
        .arg(path)
        .output()
        .await
        .context(read_rpm_metadata_error::SpawnSnafu {
            path: path.to_path_buf(),
        })?;
    snafu::ensure!(
        output.status.success(),
        read_rpm_metadata_error::CommandSnafu {
            path: path.to_path_buf()
        }
    );
    let stdout = String::from_utf8(output.stdout).context(read_rpm_metadata_error::Utf8Snafu {
        path: path.to_path_buf(),
    })?;
    parse_rpm_query_output(&stdout).context(read_rpm_metadata_error::ParseSnafu {
        path: path.to_path_buf(),
    })
}

pub async fn run(
    targets: &[RpmTarget],
    features: &[Feature],
    siblings: &[PathBuf],
    overwrite_manifest: bool,
) -> Result<(), Whatever> {
    let rpm_artifacts = crate::rpm::run(targets, features, siblings).await?;
    let meta = crate::package_meta("pishoo")?;
    let target_dir = crate::target_dir()?;
    let manifest_path = target_dir.join("common").join("rpm").join("manifest.toml");
    let exists = tokio::fs::try_exists(&manifest_path)
        .await
        .whatever_context(format!("failed to inspect {}", manifest_path.display()))?;
    if prompt::confirm_manifest_overwrite(exists, overwrite_manifest)
        .await
        .whatever_context("failed to confirm rpm package manifest overwrite")?
        == OverwriteDecision::Skip
    {
        return Ok(());
    }

    let mut artifacts = Vec::new();
    let mut package_architectures = BTreeSet::new();
    for rpm_artifact in &rpm_artifacts {
        let artifact = manifest_artifact(rpm_artifact, &target_dir).await?;
        push_unique_package_architecture(&mut artifacts, &mut package_architectures, artifact);
    }

    let manifest = PackageManifest {
        schema_version: 1,
        kind: ArtifactKind::Rpm,
        package: PACKAGE_NAME.to_string(),
        version: meta.version,
        generated_at: generated_at(),
        git_commit: None,
        git_dirty: false,
        artifacts,
    };
    super::manifest::write_manifest(&manifest_path, &manifest)
        .await
        .whatever_context("failed to write rpm package manifest")?;
    Ok(())
}

async fn manifest_artifact(
    rpm_artifact: &RpmArtifact,
    target_dir: &Path,
) -> Result<PackageArtifact, Whatever> {
    let metadata = read_rpm_metadata(&rpm_artifact.path)
        .await
        .whatever_context("failed to read rpm package metadata")?;
    let path = target_relative_artifact_path(&rpm_artifact.path, target_dir)
        .whatever_context("failed to make rpm artifact path target-relative")?;
    let sha256 = crate::sha256_file(&rpm_artifact.path).await?;
    let size = tokio::fs::metadata(&rpm_artifact.path)
        .await
        .whatever_context(format!("failed to inspect {}", rpm_artifact.path.display()))?
        .len();
    Ok(PackageArtifact {
        target: rpm_artifact.target.clone(),
        path,
        sha256,
        size,
        package_name: Some(metadata.package_name),
        package_version: Some(metadata.package_version),
        architecture: Some(metadata.architecture),
        archive_name: rpm_artifact
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .map(ToOwned::to_owned),
        features: rpm_artifact.features.clone(),
        profile: Some("release".to_string()),
    })
}

fn push_unique_package_architecture(
    artifacts: &mut Vec<PackageArtifact>,
    package_architectures: &mut BTreeSet<(String, String)>,
    artifact: PackageArtifact,
) {
    if let (Some(package), Some(architecture)) = (&artifact.package_name, &artifact.architecture)
        && !package_architectures.insert((package.clone(), architecture.clone()))
    {
        return;
    }
    artifacts.push(artifact);
}

fn target_relative_artifact_path(
    path: &Path,
    target_dir: &Path,
) -> Result<String, RpmPackageManifestError> {
    path.strip_prefix(target_dir)
        .context(rpm_package_manifest_error::TargetRelativePathSnafu)?
        .to_str()
        .map(ToOwned::to_owned)
        .ok_or(RpmPackageManifestError::ArtifactPathUtf8)
}

fn generated_at() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{PackageArtifact, parse_rpm_query_output, push_unique_package_architecture};

    #[test]
    fn parses_rpm_query_output() {
        let metadata =
            parse_rpm_query_output("gmutils\n0.5.2-1\nx86_64\n").expect("metadata should parse");

        assert_eq!(metadata.package_name, "gmutils");
        assert_eq!(metadata.package_version, "0.5.2-1");
        assert_eq!(metadata.architecture, "x86_64");
    }

    #[test]
    fn short_rpm_query_output_fails() {
        let error =
            parse_rpm_query_output("gmutils\n0.5.2-1\n").expect_err("short output should fail");
        assert_eq!(
            error.to_string(),
            "rpm metadata query returned incomplete output"
        );
    }

    #[test]
    fn duplicate_common_package_architecture_is_written_once() {
        let mut artifacts = Vec::new();
        let mut keys = BTreeSet::new();

        push_unique_package_architecture(
            &mut artifacts,
            &mut keys,
            artifact("x86_64-unknown-linux-gnu", "pishoo-common", "noarch"),
        );
        push_unique_package_architecture(
            &mut artifacts,
            &mut keys,
            artifact("aarch64-unknown-linux-gnu", "pishoo-common", "noarch"),
        );

        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].target, "x86_64-unknown-linux-gnu");
    }

    fn artifact(target: &str, package: &str, architecture: &str) -> PackageArtifact {
        PackageArtifact {
            target: target.to_string(),
            path: format!("{target}/release/rpm/{package}.rpm"),
            sha256: "0".repeat(64),
            size: 1,
            package_name: Some(package.to_string()),
            package_version: Some("0.5.2-1".to_string()),
            architecture: Some(architecture.to_string()),
            archive_name: Some(format!("{package}.rpm")),
            features: Vec::new(),
            profile: Some("release".to_string()),
        }
    }
}
