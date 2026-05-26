use std::path::{Path, PathBuf};

use snafu::{OptionExt, ResultExt, Whatever};
use tracing::info;

use super::{
    VerifyOptions,
    artifact::{ArtifactRoot, ReleaseManifest, read_manifest, sha256_file},
    paths::common_paths,
};

pub async fn run(_options: VerifyOptions) -> Result<(), Whatever> {
    let root = common_paths()?.root;
    verify_common_root(&root).await
}

async fn verify_common_root(root: &Path) -> Result<(), Whatever> {
    let manifest_path = root.join("manifest.toml");
    let manifest = read_manifest(&manifest_path).await?;
    verify_manifest_artifacts(root, &manifest).await?;
    verify_homebrew(root, &manifest).await?;
    verify_apt(root).await?;
    info!(path = %root.display(), "verified staged release artifacts");
    Ok(())
}

async fn verify_manifest_artifacts(
    root: &Path,
    manifest: &ReleaseManifest,
) -> Result<(), Whatever> {
    for artifact in &manifest.artifacts {
        let path = artifact_path(root, artifact.root, &artifact.path);
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

fn artifact_path(root: &Path, artifact_root: ArtifactRoot, path: &str) -> PathBuf {
    match artifact_root {
        ArtifactRoot::Homebrew => root.join("homebrew").join(path),
        ArtifactRoot::Apt => root.join("apt").join(path),
    }
}

async fn verify_homebrew(root: &Path, manifest: &ReleaseManifest) -> Result<(), Whatever> {
    let homebrew = root.join("homebrew");
    if !tokio::fs::try_exists(&homebrew)
        .await
        .whatever_context(format!("failed to inspect {}", homebrew.display()))?
    {
        return Ok(());
    }

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
    Ok(())
}

async fn verify_apt(root: &Path) -> Result<(), Whatever> {
    let apt = root.join("apt");
    if !tokio::fs::try_exists(&apt)
        .await
        .whatever_context(format!("failed to inspect {}", apt.display()))?
    {
        return Ok(());
    }
    let dists = apt.join("dists");
    if !tokio::fs::try_exists(&dists)
        .await
        .whatever_context(format!("failed to inspect {}", dists.display()))?
    {
        return Ok(());
    }

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
        for name in ["Release", "Release.gpg", "InRelease"] {
            let path = suite.join(name);
            snafu::ensure_whatever!(
                tokio::fs::try_exists(&path)
                    .await
                    .whatever_context(format!("failed to inspect {}", path.display()))?,
                "apt metadata {} is missing",
                path.display()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::verify_common_root;
    use crate::release::artifact::{
        ArtifactEntry, ArtifactRoot, ReleaseManifest, sha256_file, write_manifest,
    };

    #[tokio::test]
    async fn valid_manifest_artifacts_pass() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let root = temp.path();
        let homebrew = root.join("homebrew");
        tokio::fs::create_dir_all(&homebrew)
            .await
            .expect("homebrew dir should be created");
        let archive = homebrew.join("gmutils-0.5.1-aarch64-apple-darwin.tar.gz");
        tokio::fs::write(&archive, "archive")
            .await
            .expect("archive should be written");
        let formula = homebrew.join("gmutils.rb");
        tokio::fs::write(
            &formula,
            "url \"https://download.genmeta.net/homebrew/gmutils-0.5.1-aarch64-apple-darwin.tar.gz\"",
        )
        .await
        .expect("formula should be written");
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
                package: "gmutils".to_string(),
                version: "0.5.1".to_string(),
                artifacts: vec![
                    ArtifactEntry {
                        root: ArtifactRoot::Homebrew,
                        path: "gmutils-0.5.1-aarch64-apple-darwin.tar.gz".to_string(),
                        sha256: archive_sha,
                        immutable: true,
                    },
                    ArtifactEntry {
                        root: ArtifactRoot::Homebrew,
                        path: "gmutils.rb".to_string(),
                        sha256: formula_sha,
                        immutable: false,
                    },
                ],
            },
        )
        .await
        .expect("manifest should be written");

        verify_common_root(root)
            .await
            .expect("valid artifacts should verify");
    }

    #[tokio::test]
    async fn changed_file_fails_with_sha256_mismatch() {
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
                package: "gmutils".to_string(),
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

        let error = verify_common_root(root)
            .await
            .expect_err("changed file should fail");

        assert_eq!(error.to_string(), "sha256 mismatch for pishoo.rb");
    }
}
