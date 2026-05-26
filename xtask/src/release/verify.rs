use std::{collections::BTreeSet, ffi::OsString, path::Path};

use clap::{CommandFactory, Parser, Subcommand, error::ErrorKind};
use snafu::{OptionExt, ResultExt, Whatever};
use tracing::info;

use super::{
    artifact::{ArtifactRoot, ReleaseManifest, read_manifest, sha256_file},
    grouped,
    paths::common_paths,
};

#[derive(Debug, Parser)]
struct LocalCli {
    #[command(subcommand)]
    target: LocalTarget,
}

#[derive(Debug, Subcommand)]
enum LocalTarget {
    /// Verify staged Homebrew artifacts
    Homebrew,
    /// Verify staged APT artifacts
    Apt,
    /// Verify staged RPM artifacts
    Rpm,
}

impl LocalTarget {
    fn root(self) -> ArtifactRoot {
        match self {
            Self::Homebrew => ArtifactRoot::Homebrew,
            Self::Apt => ArtifactRoot::Apt,
            Self::Rpm => ArtifactRoot::Rpm,
        }
    }
}

pub fn parse_local_targets(tokens: &[OsString]) -> Result<Vec<ArtifactRoot>, clap::Error> {
    if tokens.is_empty() {
        return Ok(Vec::new());
    }

    let sections = grouped::parse_grouped_targets(tokens, &["homebrew", "apt", "rpm"])
        .map_err(|error| local_error(ErrorKind::ValueValidation, error))?;

    let mut roots = BTreeSet::new();
    for section in sections {
        let mut argv = vec![
            OsString::from("xtask verify local"),
            section.name.clone().into(),
        ];
        argv.extend(section.args);
        let cli = LocalCli::try_parse_from(argv)?;
        roots.insert(cli.target.root());
    }
    Ok(roots.into_iter().collect())
}

fn local_error(kind: ErrorKind, message: impl std::fmt::Display) -> clap::Error {
    LocalCli::command()
        .bin_name("xtask verify local")
        .error(kind, message)
}

pub async fn run_local(roots: &[ArtifactRoot]) -> Result<(), Whatever> {
    let root = common_paths()?.root;
    verify_common_root_for_roots(&root, roots).await
}

pub async fn verify_common_root_for_roots(
    root: &Path,
    roots: &[ArtifactRoot],
) -> Result<(), Whatever> {
    let manifest_path = root.join("manifest.toml");
    let manifest = read_manifest(&manifest_path).await?;
    let selected_roots = selected_roots(&manifest, roots);
    let explicit_roots = !roots.is_empty();
    if explicit_roots {
        verify_selected_roots_have_manifest_artifacts(&manifest, &selected_roots)?;
    }
    verify_manifest_artifacts(root, &manifest, &selected_roots).await?;
    if selected_roots.contains(&ArtifactRoot::Homebrew) {
        verify_homebrew(root, &manifest, explicit_roots).await?;
    }
    if selected_roots.contains(&ArtifactRoot::Apt) {
        verify_apt(root, explicit_roots).await?;
    }
    if selected_roots.contains(&ArtifactRoot::Rpm) {
        verify_rpm(root, &manifest).await?;
    }
    info!(path = %root.display(), "verified staged release artifacts");
    Ok(())
}

fn selected_roots(manifest: &ReleaseManifest, roots: &[ArtifactRoot]) -> BTreeSet<ArtifactRoot> {
    if roots.is_empty() {
        manifest
            .artifacts
            .iter()
            .map(|artifact| artifact.root)
            .collect()
    } else {
        roots.iter().copied().collect()
    }
}

fn verify_selected_roots_have_manifest_artifacts(
    manifest: &ReleaseManifest,
    selected_roots: &BTreeSet<ArtifactRoot>,
) -> Result<(), Whatever> {
    for root in selected_roots {
        snafu::ensure_whatever!(
            manifest
                .artifacts
                .iter()
                .any(|artifact| artifact.root == *root),
            "selected release target {} has no manifest artifacts",
            root.directory()
        );
    }
    Ok(())
}

async fn verify_manifest_artifacts(
    root: &Path,
    manifest: &ReleaseManifest,
    selected_roots: &BTreeSet<ArtifactRoot>,
) -> Result<(), Whatever> {
    for artifact in manifest
        .artifacts
        .iter()
        .filter(|artifact| selected_roots.contains(&artifact.root))
    {
        let path = root.join(artifact.root.directory()).join(&artifact.path);
        snafu::ensure_whatever!(
            tokio::fs::try_exists(&path)
                .await
                .whatever_context(format!("failed to inspect {}", path.display()))?,
            "artifact {} is missing",
            artifact.path
        );
        let actual = sha256_file(&path).await?;
        snafu::ensure_whatever!(
            actual == artifact.sha256,
            "sha256 mismatch for {}",
            artifact.path
        );
    }
    Ok(())
}

async fn verify_homebrew(
    root: &Path,
    manifest: &ReleaseManifest,
    required: bool,
) -> Result<(), Whatever> {
    let homebrew = root.join("homebrew");
    if !tokio::fs::try_exists(&homebrew)
        .await
        .whatever_context(format!("failed to inspect {}", homebrew.display()))?
    {
        snafu::ensure_whatever!(
            !required,
            "homebrew target is missing at {}",
            homebrew.display()
        );
        return Ok(());
    }

    let mut found_formula = false;
    let mut entries = tokio::fs::read_dir(&homebrew)
        .await
        .whatever_context(format!("failed to read {}", homebrew.display()))?;
    while let Some(entry) = entries
        .next_entry()
        .await
        .whatever_context(format!("failed to read entry in {}", homebrew.display()))?
    {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("rb") {
            continue;
        }
        found_formula = true;
        let package = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .whatever_context("failed to read homebrew formula file stem as utf-8")?;
        let content = tokio::fs::read_to_string(&path)
            .await
            .whatever_context(format!("failed to read {}", path.display()))?;
        for archive in manifest.artifacts.iter().filter(|artifact| {
            artifact.root == ArtifactRoot::Homebrew
                && artifact.immutable
                && artifact.path.ends_with(".tar.gz")
                && artifact.path.starts_with(&format!("{package}-"))
        }) {
            snafu::ensure_whatever!(
                content.contains(&archive.path),
                "homebrew formula {} does not reference {}",
                path.display(),
                archive.path
            );
        }
    }
    snafu::ensure_whatever!(
        !required || found_formula,
        "homebrew target must contain at least one formula"
    );
    Ok(())
}

async fn verify_rpm(root: &Path, manifest: &ReleaseManifest) -> Result<(), Whatever> {
    let rpm = root.join(ArtifactRoot::Rpm.directory());
    snafu::ensure_whatever!(
        tokio::fs::try_exists(&rpm)
            .await
            .whatever_context(format!("failed to inspect {}", rpm.display()))?,
        "rpm root is missing at {}",
        rpm.display()
    );
    let has_recorded_rpm = manifest.artifacts.iter().any(|artifact| {
        artifact.root == ArtifactRoot::Rpm && artifact.immutable && artifact.path.ends_with(".rpm")
    });
    snafu::ensure_whatever!(
        has_recorded_rpm,
        "rpm root must contain at least one immutable .rpm artifact"
    );
    Ok(())
}

async fn verify_apt(root: &Path, required: bool) -> Result<(), Whatever> {
    let apt = root.join("apt");
    if !tokio::fs::try_exists(&apt)
        .await
        .whatever_context(format!("failed to inspect {}", apt.display()))?
    {
        snafu::ensure_whatever!(!required, "apt target is missing at {}", apt.display());
        return Ok(());
    }
    let dists = apt.join("dists");
    if !tokio::fs::try_exists(&dists)
        .await
        .whatever_context(format!("failed to inspect {}", dists.display()))?
    {
        snafu::ensure_whatever!(
            !required,
            "apt target must contain dists metadata at {}",
            dists.display()
        );
        return Ok(());
    }

    let mut found_suite = false;
    let mut suites = tokio::fs::read_dir(&dists)
        .await
        .whatever_context(format!("failed to read {}", dists.display()))?;
    while let Some(entry) = suites
        .next_entry()
        .await
        .whatever_context(format!("failed to read entry in {}", dists.display()))?
    {
        let suite = entry.path();
        let file_type = entry
            .file_type()
            .await
            .whatever_context(format!("failed to inspect {}", suite.display()))?;
        if !file_type.is_dir() {
            continue;
        }
        found_suite = true;
        for name in ["Release", "Release.gpg", "InRelease"] {
            let path = suite.join(name);
            snafu::ensure_whatever!(
                tokio::fs::try_exists(&path)
                    .await
                    .whatever_context(format!("failed to inspect {}", path.display()))?,
                "apt suite {} is missing {}",
                suite.display(),
                name
            );
        }
    }
    snafu::ensure_whatever!(
        !required || found_suite,
        "apt target must contain at least one suite"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{parse_local_targets, verify_common_root_for_roots};
    use crate::release::artifact::{
        ArtifactEntry, ArtifactRoot, ReleaseManifest, sha256_file, write_manifest,
    };

    #[tokio::test]
    async fn valid_homebrew_and_apt_artifacts_verify() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root = temp.path();
        let homebrew = root.join("homebrew");
        tokio::fs::create_dir_all(&homebrew)
            .await
            .expect("homebrew dir should be created");
        let archive = homebrew.join("pishoo-0.5.1-aarch64-apple-darwin.tar.gz");
        tokio::fs::write(&archive, "archive")
            .await
            .expect("archive should be written");
        let formula = homebrew.join("pishoo.rb");
        tokio::fs::write(
            &formula,
            "url \"https://download.genmeta.net/homebrew/pishoo-0.5.1-aarch64-apple-darwin.tar.gz\"",
        )
        .await
        .expect("formula should be written");
        let apt_suite = root.join("apt/dists/stable");
        tokio::fs::create_dir_all(&apt_suite)
            .await
            .expect("apt suite should be created");
        for name in ["Release", "Release.gpg", "InRelease"] {
            tokio::fs::write(apt_suite.join(name), name)
                .await
                .expect("apt metadata should be written");
        }
        let archive_sha = sha256_file(&archive)
            .await
            .expect("archive should be hashed");
        let formula_sha = sha256_file(&formula)
            .await
            .expect("formula should be hashed");
        write_manifest(
            &root.join("manifest.toml"),
            &ReleaseManifest {
                schema_version: 1,
                package: "pishoo".to_string(),
                version: "0.5.1".to_string(),
                artifacts: vec![
                    ArtifactEntry {
                        root: ArtifactRoot::Homebrew,
                        path: "pishoo-0.5.1-aarch64-apple-darwin.tar.gz".to_string(),
                        sha256: archive_sha,
                        immutable: true,
                    },
                    ArtifactEntry {
                        root: ArtifactRoot::Homebrew,
                        path: "pishoo.rb".to_string(),
                        sha256: formula_sha,
                        immutable: false,
                    },
                ],
            },
        )
        .await
        .expect("manifest should be written");

        verify_common_root_for_roots(root, &[])
            .await
            .expect("valid artifacts should verify");
    }

    #[tokio::test]
    async fn changed_homebrew_file_fails_with_sha256_mismatch() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root = temp.path();
        let homebrew = root.join("homebrew");
        tokio::fs::create_dir_all(&homebrew)
            .await
            .expect("homebrew dir should be created");
        let file = homebrew.join("pishoo.rb");
        tokio::fs::write(&file, "before")
            .await
            .expect("file should be written");
        let sha = sha256_file(&file).await.expect("file should be hashed");
        write_manifest(
            &root.join("manifest.toml"),
            &ReleaseManifest {
                schema_version: 1,
                package: "pishoo".to_string(),
                version: "0.5.1".to_string(),
                artifacts: vec![ArtifactEntry {
                    root: ArtifactRoot::Homebrew,
                    path: "pishoo.rb".to_string(),
                    sha256: sha,
                    immutable: false,
                }],
            },
        )
        .await
        .expect("manifest should be written");
        tokio::fs::write(&file, "after")
            .await
            .expect("file should be changed");

        let error = verify_common_root_for_roots(root, &[])
            .await
            .expect_err("changed file should fail");

        assert_eq!(error.to_string(), "sha256 mismatch for pishoo.rb");
    }

    #[test]
    fn verify_local_no_targets_selects_manifest_roots() {
        let roots = parse_local_targets(&[]).expect("empty local targets should parse");

        assert!(roots.is_empty());
    }

    #[tokio::test]
    async fn verify_selected_roots_ignores_unselected_missing_artifacts() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root = temp.path();
        let homebrew = root.join("homebrew");
        tokio::fs::create_dir_all(&homebrew)
            .await
            .expect("homebrew dir should be created");
        let archive = homebrew.join("pishoo-0.5.1-aarch64-apple-darwin.tar.gz");
        tokio::fs::write(&archive, "archive")
            .await
            .expect("archive should be written");
        let formula = homebrew.join("pishoo.rb");
        tokio::fs::write(
            &formula,
            "url \"https://download.genmeta.net/homebrew/pishoo-0.5.1-aarch64-apple-darwin.tar.gz\"",
        )
        .await
        .expect("formula should be written");
        let archive_sha = sha256_file(&archive)
            .await
            .expect("archive should be hashed");
        write_manifest(
            &root.join("manifest.toml"),
            &ReleaseManifest {
                schema_version: 1,
                package: "pishoo".to_string(),
                version: "0.5.1".to_string(),
                artifacts: vec![
                    ArtifactEntry {
                        root: ArtifactRoot::Homebrew,
                        path: "pishoo-0.5.1-aarch64-apple-darwin.tar.gz".to_string(),
                        sha256: archive_sha,
                        immutable: true,
                    },
                    ArtifactEntry {
                        root: ArtifactRoot::Apt,
                        path: "missing.deb".to_string(),
                        sha256: "not-a-real-hash".to_string(),
                        immutable: true,
                    },
                ],
            },
        )
        .await
        .expect("manifest should be written");

        verify_common_root_for_roots(root, &[ArtifactRoot::Homebrew])
            .await
            .expect("selected root should ignore unselected missing artifacts");
    }

    #[tokio::test]
    async fn verify_rpm_requires_recorded_rpm_file() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root = temp.path();
        let rpm = root.join("rpm");
        tokio::fs::create_dir_all(&rpm)
            .await
            .expect("rpm dir should be created");
        let file = rpm.join("pishoo-0.5.1-1.x86_64.rpm");
        tokio::fs::write(&file, "rpm")
            .await
            .expect("rpm should be written");
        let sha = sha256_file(&file).await.expect("rpm should be hashed");
        write_manifest(
            &root.join("manifest.toml"),
            &ReleaseManifest {
                schema_version: 1,
                package: "pishoo".to_string(),
                version: "0.5.1".to_string(),
                artifacts: vec![ArtifactEntry {
                    root: ArtifactRoot::Rpm,
                    path: "pishoo-0.5.1-1.x86_64.rpm".to_string(),
                    sha256: sha,
                    immutable: true,
                }],
            },
        )
        .await
        .expect("manifest should be written");

        verify_common_root_for_roots(root, &[ArtifactRoot::Rpm])
            .await
            .expect("recorded rpm should verify");
    }

    #[tokio::test]
    async fn verify_rpm_requires_manifest_entry() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root = temp.path();
        let rpm = root.join("rpm");
        tokio::fs::create_dir_all(&rpm)
            .await
            .expect("rpm dir should be created");
        let file = rpm.join("pishoo-0.5.1-1.x86_64.rpm");
        tokio::fs::write(&file, "rpm")
            .await
            .expect("rpm should be written");
        let sha = sha256_file(&file).await.expect("rpm should be hashed");
        write_manifest(
            &root.join("manifest.toml"),
            &ReleaseManifest {
                schema_version: 1,
                package: "pishoo".to_string(),
                version: "0.5.1".to_string(),
                artifacts: vec![ArtifactEntry {
                    root: ArtifactRoot::Rpm,
                    path: "pishoo-0.5.1-1.x86_64.rpm".to_string(),
                    sha256: sha,
                    immutable: false,
                }],
            },
        )
        .await
        .expect("manifest should be written");

        let error = verify_common_root_for_roots(root, &[ArtifactRoot::Rpm])
            .await
            .expect_err("selected rpm without manifest entry should fail");

        assert_eq!(
            error.to_string(),
            "rpm root must contain at least one immutable .rpm artifact"
        );
    }

    #[tokio::test]
    async fn verify_explicit_selected_root_requires_manifest_artifacts() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root = temp.path();
        write_manifest(
            &root.join("manifest.toml"),
            &ReleaseManifest {
                schema_version: 1,
                package: "pishoo".to_string(),
                version: "0.5.1".to_string(),
                artifacts: Vec::new(),
            },
        )
        .await
        .expect("manifest should be written");

        let error = verify_common_root_for_roots(root, &[ArtifactRoot::Apt])
            .await
            .expect_err("explicit selected root without manifest artifacts should fail");

        assert_eq!(
            error.to_string(),
            "selected release target apt has no manifest artifacts"
        );
    }
}
