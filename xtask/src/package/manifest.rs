#![allow(dead_code)]

use std::{
    collections::BTreeSet,
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use snafu::{ResultExt, Snafu};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactKind {
    Deb,
    Rpm,
    Brew,
}

impl ArtifactKind {
    pub fn directory(self) -> &'static str {
        match self {
            Self::Deb => "deb",
            Self::Rpm => "rpm",
            Self::Brew => "brew",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct PackageManifest {
    pub schema_version: u32,
    pub kind: ArtifactKind,
    pub package: String,
    pub version: String,
    pub generated_at: String,
    pub git_commit: Option<String>,
    pub git_dirty: bool,
    pub artifacts: Vec<PackageArtifact>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct PackageArtifact {
    pub target: String,
    pub path: String,
    pub sha256: String,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub architecture: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum ValidateManifestError {
    #[snafu(display("artifact path must be target-relative"))]
    AbsolutePath,
    #[snafu(display("artifact path must not contain parent components"))]
    ParentComponent,
    #[snafu(display("linux package artifact must include package name"))]
    MissingPackageName,
    #[snafu(display("linux package artifact must include architecture"))]
    MissingArchitecture,
    #[snafu(display("duplicate package artifact for {package} {architecture}"))]
    DuplicatePackageArchitecture {
        package: String,
        architecture: String,
    },
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum ManifestIoError {
    #[snafu(display("failed to read package manifest"))]
    Read {
        source: std::io::Error,
        path: PathBuf,
    },
    #[snafu(display("failed to parse package manifest"))]
    Parse {
        source: toml::de::Error,
        path: PathBuf,
    },
    #[snafu(display("failed to serialize package manifest"))]
    Serialize { source: toml::ser::Error },
    #[snafu(display("failed to create package manifest directory"))]
    CreateParent {
        source: std::io::Error,
        path: PathBuf,
    },
    #[snafu(display("failed to write package manifest"))]
    Write {
        source: std::io::Error,
        path: PathBuf,
    },
}

pub fn validate_manifest(manifest: &PackageManifest) -> Result<(), ValidateManifestError> {
    let mut linux_keys = BTreeSet::new();
    for artifact in &manifest.artifacts {
        validate_target_relative_path(&artifact.path)?;
        if matches!(manifest.kind, ArtifactKind::Deb | ArtifactKind::Rpm) {
            let package = artifact
                .package_name
                .clone()
                .ok_or(ValidateManifestError::MissingPackageName)?;
            let architecture = artifact
                .architecture
                .clone()
                .ok_or(ValidateManifestError::MissingArchitecture)?;
            if !linux_keys.insert((package.clone(), architecture.clone())) {
                return Err(ValidateManifestError::DuplicatePackageArchitecture {
                    package,
                    architecture,
                });
            }
        }
    }
    Ok(())
}

pub fn validate_target_relative_path(value: &str) -> Result<(), ValidateManifestError> {
    let path = Path::new(value);
    if path.is_absolute() {
        return Err(ValidateManifestError::AbsolutePath);
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(ValidateManifestError::ParentComponent);
    }
    Ok(())
}

pub async fn read_manifest(path: &Path) -> Result<PackageManifest, ManifestIoError> {
    let content = tokio::fs::read_to_string(path)
        .await
        .context(manifest_io_error::ReadSnafu {
            path: path.to_path_buf(),
        })?;
    let manifest = toml::from_str(&content).context(manifest_io_error::ParseSnafu {
        path: path.to_path_buf(),
    })?;
    Ok(manifest)
}

pub async fn write_manifest(
    path: &Path,
    manifest: &PackageManifest,
) -> Result<(), ManifestIoError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .context(manifest_io_error::CreateParentSnafu {
                path: parent.to_path_buf(),
            })?;
    }
    let content = toml::to_string_pretty(manifest).context(manifest_io_error::SerializeSnafu)?;
    tokio::fs::write(path, content)
        .await
        .context(manifest_io_error::WriteSnafu {
            path: path.to_path_buf(),
        })
}

#[cfg(test)]
mod tests {
    use super::{ArtifactKind, PackageArtifact, PackageManifest, validate_manifest};

    fn artifact(path: &str, package: &str, arch: &str) -> PackageArtifact {
        PackageArtifact {
            target: "x86_64-unknown-linux-gnu".to_string(),
            path: path.to_string(),
            sha256: "0".repeat(64),
            size: 42,
            package_name: Some(package.to_string()),
            package_version: Some("0.5.2-1".to_string()),
            architecture: Some(arch.to_string()),
            archive_name: None,
            features: Vec::new(),
            profile: Some("release".to_string()),
        }
    }

    #[test]
    fn manifest_round_trips_as_toml() {
        let manifest = PackageManifest {
            schema_version: 1,
            kind: ArtifactKind::Deb,
            package: "gmutils".to_string(),
            version: "0.5.2".to_string(),
            generated_at: "2026-05-27T00:00:00Z".to_string(),
            git_commit: Some("abcdef".to_string()),
            git_dirty: false,
            artifacts: vec![artifact(
                "x86_64-unknown-linux-gnu/release/deb/gmutils_0.5.2-1_amd64.deb",
                "gmutils",
                "amd64",
            )],
        };

        let encoded = toml::to_string_pretty(&manifest).expect("manifest should encode");
        assert!(encoded.contains("kind = \"deb\""));
        let decoded: PackageManifest = toml::from_str(&encoded).expect("manifest should decode");
        assert_eq!(decoded, manifest);
    }

    #[test]
    fn absolute_artifact_path_is_rejected() {
        let mut manifest = PackageManifest {
            schema_version: 1,
            kind: ArtifactKind::Deb,
            package: "gmutils".to_string(),
            version: "0.5.2".to_string(),
            generated_at: "2026-05-27T00:00:00Z".to_string(),
            git_commit: None,
            git_dirty: false,
            artifacts: vec![artifact(
                "/tmp/gmutils_0.5.2-1_amd64.deb",
                "gmutils",
                "amd64",
            )],
        };

        let error = validate_manifest(&manifest).expect_err("absolute path should fail");
        assert_eq!(error.to_string(), "artifact path must be target-relative");

        manifest.artifacts[0].path = "../release/gmutils.deb".to_string();
        let error = validate_manifest(&manifest).expect_err("parent path should fail");
        assert_eq!(
            error.to_string(),
            "artifact path must not contain parent components"
        );
    }

    #[test]
    fn duplicate_linux_package_architecture_is_rejected() {
        let manifest = PackageManifest {
            schema_version: 1,
            kind: ArtifactKind::Deb,
            package: "gmutils".to_string(),
            version: "0.5.2".to_string(),
            generated_at: "2026-05-27T00:00:00Z".to_string(),
            git_commit: None,
            git_dirty: false,
            artifacts: vec![
                artifact(
                    "x86_64-unknown-linux-gnu/release/deb/a.deb",
                    "gmutils",
                    "amd64",
                ),
                artifact(
                    "x86_64-unknown-linux-gnu/release/deb/b.deb",
                    "gmutils",
                    "amd64",
                ),
            ],
        };

        let error = validate_manifest(&manifest).expect_err("duplicate key should fail");
        assert_eq!(
            error.to_string(),
            "duplicate package artifact for gmutils amd64"
        );
    }
}
