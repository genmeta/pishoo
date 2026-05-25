use std::path::{Path, PathBuf};

use serde::Deserialize;
use snafu::{OptionExt, ResultExt, Whatever};
use tracing::info;

use super::{
    artifact::{
        ArtifactEntry, ArtifactRoot, ReleaseManifest, copy_artifact, read_manifest, sha256_file,
        write_manifest,
    },
    paths::{common_paths, promote_staged_outputs, recreate_dir},
};
use crate::{package_meta, target_dir};

const CARGO_NAME: &str = "pishoo";
const BREW_DL_URL: &str = "https://download.genmeta.net/homebrew";
const BREW_FEATURES_FILE: &str = "pishoo-brew-features.toml";
const SUPPORTED_TRIPLES: [&str; 2] = ["aarch64-apple-darwin", "x86_64-apple-darwin"];

#[derive(Debug, Clone)]
struct ArchiveInfo {
    triple: String,
    archive_name: String,
    sha256: String,
}

#[derive(Debug)]
struct ArchiveSource {
    triple: String,
    archive_name: String,
    source: PathBuf,
    features: BrewFeatures,
}

#[derive(Debug)]
struct HomebrewInputs {
    archives: Vec<ArchiveSource>,
    content: String,
    features: BrewFeatures,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
struct BrewFeatures {
    sshd: bool,
    pam: bool,
}

impl BrewFeatures {
    fn includes_ssh_session(self) -> bool {
        self.sshd || self.pam
    }
}

fn brew_on_block(triple: &str) -> Result<&'static str, Whatever> {
    match triple {
        "aarch64-apple-darwin" => Ok("on_arm"),
        "x86_64-apple-darwin" => Ok("on_intel"),
        _ => snafu::whatever!("unsupported brew target triple: {triple}"),
    }
}

fn generate_formula(
    name: &str,
    description: &str,
    version: &str,
    homepage: &str,
    license: &str,
    archives: &[ArchiveInfo],
    content: &str,
) -> Result<String, Whatever> {
    let class_name = {
        let mut chars = name.chars();
        match chars.next() {
            Some(c) => c.to_uppercase().to_string() + chars.as_str(),
            None => String::new(),
        }
    };
    let desc = description.replace('"', "\\\"");
    let homepage = homepage.replace('"', "\\\"");
    let license = license.replace('"', "\\\"");

    let mut lines = vec![
        format!("class {class_name} < Formula"),
        format!("  desc \"{desc}\""),
        format!("  version \"{version}\""),
        format!("  homepage \"{homepage}\""),
    ];
    if !license.is_empty() {
        lines.push(format!("  license \"{license}\""));
    }
    lines.push(String::new());

    for info in archives {
        let block = brew_on_block(&info.triple)?;
        lines.extend([
            format!("  {block} do"),
            format!("    url \"{BREW_DL_URL}/{}\"", info.archive_name),
            format!("    sha256 \"{}\"", info.sha256),
            "  end".to_string(),
            String::new(),
        ]);
    }

    lines.push(content.trim_end().to_string());
    lines.push("end".to_string());
    lines.push(String::new());

    Ok(lines.join("\n"))
}

pub async fn stage() -> Result<(), Whatever> {
    info!("starting homebrew stage");
    let meta = package_meta(CARGO_NAME)?;
    let workspace_root = workspace_root()?;
    let target_dir = target_dir()?;
    let paths = common_paths()?;
    let inputs = validate_homebrew_inputs(&target_dir, &workspace_root, &meta.version).await?;

    let manifest = read_existing_manifest(&paths.manifest, &meta.version).await?;
    let staging = paths.root.join("homebrew.staging");
    recreate_dir(&staging).await?;

    let mut archives = Vec::new();
    for archive in inputs.archives {
        let destination = staging.join(&archive.archive_name);
        copy_artifact(&archive.source, &destination).await?;
        let sha256 = sha256_file(&destination).await?;
        info!(path = %destination.display(), "staged homebrew archive");
        archives.push(ArchiveInfo {
            triple: archive.triple,
            archive_name: archive.archive_name,
            sha256,
        });
    }

    let formula_content = formula_content_for_features(&inputs.content, inputs.features);
    let formula = generate_formula(
        CARGO_NAME,
        &meta.description,
        &meta.version,
        &meta.homepage,
        &meta.license,
        &archives,
        &formula_content,
    )?;
    let formula_path = staging.join(format!("{CARGO_NAME}.rb"));
    tokio::fs::write(&formula_path, formula)
        .await
        .whatever_context(format!("failed to write {}", formula_path.display()))?;
    let formula_sha256 = sha256_file(&formula_path).await?;

    let manifest = merge_homebrew_manifest(
        manifest,
        &meta.version,
        archives,
        format!("{CARGO_NAME}.rb"),
        formula_sha256,
    );
    let manifest_staging = paths.root.join("manifest.toml.staging");
    write_manifest(&manifest_staging, &manifest).await?;

    promote_staged_outputs(
        "homebrew",
        &staging,
        &paths.homebrew,
        &manifest_staging,
        &paths.manifest,
    )
    .await?;
    info!(path = %paths.homebrew.join(format!("{CARGO_NAME}.rb")).display(), "staged homebrew formula");

    info!("finished homebrew stage");
    Ok(())
}

fn workspace_root() -> Result<PathBuf, Whatever> {
    let metadata = cargo_metadata::MetadataCommand::new()
        .no_deps()
        .exec()
        .whatever_context("failed to read cargo metadata")?;
    Ok(metadata.workspace_root.into_std_path_buf())
}

async fn validate_homebrew_inputs(
    target_dir: &Path,
    workspace_root: &Path,
    version: &str,
) -> Result<HomebrewInputs, Whatever> {
    let mut archives = Vec::new();
    for triple in SUPPORTED_TRIPLES {
        let archive_name = format!("{CARGO_NAME}_{version}-{triple}.tar.gz");
        let directory = target_dir.join(triple).join("release").join("brew");
        let source = directory.join(&archive_name);
        if tokio::fs::try_exists(&source)
            .await
            .whatever_context(format!("failed to inspect {}", source.display()))?
        {
            let features = read_features(&directory.join(BREW_FEATURES_FILE)).await?;
            archives.push(ArchiveSource {
                triple: triple.to_string(),
                archive_name,
                source,
                features,
            });
        } else {
            info!(path = %source.display(), "skipping missing homebrew archive");
        }
    }

    snafu::ensure_whatever!(
        !archives.is_empty(),
        "no homebrew archives found in target directories"
    );
    let features = archive_features(&archives)?;

    let content_path = workspace_root.join(CARGO_NAME).join("homebrew_content.rb");
    let content = tokio::fs::read_to_string(&content_path)
        .await
        .whatever_context(format!("failed to read {}", content_path.display()))?;

    Ok(HomebrewInputs {
        archives,
        content,
        features,
    })
}

async fn read_features(path: &Path) -> Result<BrewFeatures, Whatever> {
    let content = tokio::fs::read_to_string(path)
        .await
        .whatever_context(format!("failed to read {}", path.display()))?;
    toml::from_str(&content).whatever_context(format!("failed to parse {}", path.display()))
}

fn archive_features(archives: &[ArchiveSource]) -> Result<BrewFeatures, Whatever> {
    let features = archives
        .first()
        .map(|archive| archive.features)
        .whatever_context("no homebrew archives found in target directories")?;
    snafu::ensure_whatever!(
        archives.iter().all(|archive| archive.features == features),
        "homebrew archive feature sidecars disagree"
    );
    Ok(features)
}

fn formula_content_for_features(content: &str, features: BrewFeatures) -> String {
    if features.includes_ssh_session() {
        return content.trim_end().to_string();
    }
    content
        .lines()
        .filter(|line| !line.contains("pishoo-ssh-session"))
        .collect::<Vec<_>>()
        .join("\n")
}

async fn read_existing_manifest(path: &Path, version: &str) -> Result<ReleaseManifest, Whatever> {
    if tokio::fs::try_exists(path)
        .await
        .whatever_context(format!("failed to inspect {}", path.display()))?
    {
        read_manifest(path).await
    } else {
        Ok(ReleaseManifest {
            schema_version: 1,
            package: CARGO_NAME.to_string(),
            version: version.to_string(),
            artifacts: Vec::new(),
        })
    }
}

fn merge_homebrew_manifest(
    mut manifest: ReleaseManifest,
    version: &str,
    archives: Vec<ArchiveInfo>,
    formula_path: String,
    formula_sha256: String,
) -> ReleaseManifest {
    manifest.package = CARGO_NAME.to_string();
    manifest.version = version.to_string();
    manifest
        .artifacts
        .retain(|artifact| artifact.root != ArtifactRoot::Homebrew);

    for archive in archives {
        manifest.artifacts.push(ArtifactEntry {
            root: ArtifactRoot::Homebrew,
            path: archive.archive_name,
            sha256: archive.sha256,
            immutable: true,
        });
    }

    manifest.artifacts.push(ArtifactEntry {
        root: ArtifactRoot::Homebrew,
        path: formula_path,
        sha256: formula_sha256,
        immutable: false,
    });

    manifest
}

#[cfg(test)]
mod tests {
    use super::{BrewFeatures, formula_content_for_features};

    const CONTENT: &str = r#"  def install
    bin.install "pishoo"
    bin.install "pishoo-ssh-session"
  end
"#;

    #[test]
    fn formula_content_keeps_ssh_session_when_sshd_enabled() {
        let content = formula_content_for_features(
            CONTENT,
            BrewFeatures {
                sshd: true,
                pam: false,
            },
        );

        assert!(content.contains("pishoo-ssh-session"));
    }

    #[test]
    fn formula_content_strips_ssh_session_when_sshd_disabled() {
        let content = formula_content_for_features(
            CONTENT,
            BrewFeatures {
                sshd: false,
                pam: false,
            },
        );

        assert!(!content.contains("pishoo-ssh-session"));
        assert!(content.contains("bin.install \"pishoo\""));
    }
}
