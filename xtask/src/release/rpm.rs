use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use snafu::{OptionExt, ResultExt, Whatever};
use tracing::info;

use super::{
    artifact::{
        ArtifactEntry, ArtifactRoot, ReleaseManifest, copy_artifact, read_manifest, relative_path,
        sha256_file, write_manifest,
    },
    paths::{common_paths, promote_staged_outputs, recreate_dir},
};
use crate::target_dir;

const PACKAGE_NAME: &str = "pishoo";
const COMMON_PACKAGE_NAME: &str = "pishoo-common";
const RPM_SEARCH_DIRS: [&str; 4] = [
    "x86_64-unknown-linux-gnu/release/rpm",
    "aarch64-unknown-linux-gnu/release/rpm",
    "armv7-unknown-linux-gnueabihf/release/rpm",
    "i686-unknown-linux-gnu/release/rpm",
];

#[derive(Debug)]
struct RpmSource {
    package: String,
    version: String,
    filename: String,
    source: PathBuf,
}

#[derive(Debug, Clone)]
struct RpmInfo {
    path: String,
    sha256: String,
}

pub async fn stage() -> Result<(), Whatever> {
    info!("starting rpm stage");

    let target_dir = target_dir()?;
    let paths = common_paths()?;
    let rpms = discover_rpms(&target_dir).await?;
    let version = release_version(&rpms)?;
    validate_unique_artifact_paths(&rpms, &version)?;
    let manifest = read_existing_manifest(&paths.manifest).await?;
    let staging = paths.root.join("rpm.staging");
    recreate_dir(&staging).await?;

    let mut rpm_infos = Vec::new();
    for rpm in rpms {
        let relative = rpm_artifact_path(&version, &rpm.filename);
        let destination = staging.join(&relative);
        copy_artifact(&rpm.source, &destination).await?;
        let sha256 = sha256_file(&destination).await?;
        let path = relative_path(&staging, &destination)?;
        info!(path = %destination.display(), "staged rpm package");
        rpm_infos.push(RpmInfo { path, sha256 });
    }

    let manifest = merge_rpm_manifest(manifest, &version, rpm_infos);
    let manifest_staging = paths.root.join("manifest.toml.staging");
    write_manifest(&manifest_staging, &manifest).await?;

    promote_staged_outputs(
        "rpm",
        &staging,
        &paths.rpm,
        &manifest_staging,
        &paths.manifest,
    )
    .await?;

    info!(path = %paths.rpm.display(), "finished rpm stage");
    Ok(())
}

async fn discover_rpms(target_dir: &Path) -> Result<Vec<RpmSource>, Whatever> {
    let mut rpms = Vec::new();
    for relative in RPM_SEARCH_DIRS {
        let directory = target_dir.join(relative);
        if !tokio::fs::try_exists(&directory)
            .await
            .whatever_context(format!("failed to inspect {}", directory.display()))?
        {
            info!(path = %directory.display(), "skipping missing rpm directory");
            continue;
        }

        let mut entries = tokio::fs::read_dir(&directory)
            .await
            .whatever_context(format!("failed to read {}", directory.display()))?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .whatever_context(format!("failed to read entry in {}", directory.display()))?
        {
            let path = entry.path();
            let file_type = entry
                .file_type()
                .await
                .whatever_context(format!("failed to inspect {}", path.display()))?;
            if !file_type.is_file()
                || path.extension().and_then(|extension| extension.to_str()) != Some("rpm")
            {
                continue;
            }
            let filename = path
                .file_name()
                .and_then(|name| name.to_str())
                .whatever_context("failed to read rpm filename as utf-8")?
                .to_string();
            let (package, version) = parse_rpm_filename(&filename)?;
            rpms.push(RpmSource {
                package,
                version,
                filename,
                source: path,
            });
        }
    }

    snafu::ensure_whatever!(
        !rpms.is_empty(),
        "no rpm packages found in target directories"
    );
    Ok(rpms)
}

fn parse_rpm_filename(filename: &str) -> Result<(String, String), Whatever> {
    let stem = filename
        .strip_suffix(".rpm")
        .whatever_context(format!("failed to infer rpm stem from {filename}"))?;
    let (name_version_release, _) = stem
        .rsplit_once('.')
        .whatever_context(format!("failed to infer rpm architecture from {filename}"))?;
    let (name_version, _) = name_version_release
        .rsplit_once('-')
        .whatever_context(format!("failed to infer rpm release from {filename}"))?;

    for package in [COMMON_PACKAGE_NAME, PACKAGE_NAME] {
        if let Some(version) = name_version.strip_prefix(&format!("{package}-")) {
            return Ok((package.to_string(), version.to_string()));
        }
    }

    snafu::whatever!("failed to infer package version from {filename}")
}

fn release_version(rpms: &[RpmSource]) -> Result<String, Whatever> {
    let version = rpms
        .first()
        .whatever_context("no rpm packages found in target directories")?
        .version
        .clone();
    snafu::ensure_whatever!(
        rpms.iter()
            .all(|rpm| rpm.package == PACKAGE_NAME || rpm.package == COMMON_PACKAGE_NAME),
        "rpm packages contain unexpected package names"
    );
    snafu::ensure_whatever!(
        rpms.iter().all(|rpm| rpm.version == version),
        "rpm packages contain multiple release versions"
    );
    Ok(version)
}

fn rpm_artifact_path(version: &str, filename: &str) -> PathBuf {
    PathBuf::from(PACKAGE_NAME).join(version).join(filename)
}

fn validate_unique_artifact_paths(rpms: &[RpmSource], version: &str) -> Result<(), Whatever> {
    let mut seen = HashSet::new();
    for rpm in rpms {
        let path = rpm_artifact_path(version, &rpm.filename);
        let path = path
            .components()
            .map(|component| {
                component
                    .as_os_str()
                    .to_str()
                    .whatever_context("failed to convert rpm artifact path component to utf-8")
            })
            .collect::<Result<Vec<_>, _>>()?
            .join("/");
        snafu::ensure_whatever!(
            seen.insert(path.clone()),
            "duplicate rpm artifact path {path}"
        );
    }
    Ok(())
}

async fn read_existing_manifest(path: &Path) -> Result<ReleaseManifest, Whatever> {
    if tokio::fs::try_exists(path)
        .await
        .whatever_context(format!("failed to inspect {}", path.display()))?
    {
        read_manifest(path).await
    } else {
        Ok(ReleaseManifest {
            schema_version: 1,
            package: PACKAGE_NAME.to_string(),
            version: String::new(),
            artifacts: Vec::new(),
        })
    }
}

fn merge_rpm_manifest(
    mut manifest: ReleaseManifest,
    version: &str,
    rpms: Vec<RpmInfo>,
) -> ReleaseManifest {
    manifest.package = PACKAGE_NAME.to_string();
    manifest.version = version.to_string();
    manifest
        .artifacts
        .retain(|artifact| artifact.root != ArtifactRoot::Rpm);
    manifest
        .artifacts
        .extend(rpms.into_iter().map(|rpm| ArtifactEntry {
            root: ArtifactRoot::Rpm,
            path: rpm.path,
            sha256: rpm.sha256,
            immutable: true,
        }));
    manifest
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{
        RpmInfo, RpmSource, merge_rpm_manifest, parse_rpm_filename, release_version,
        rpm_artifact_path, validate_unique_artifact_paths,
    };
    use crate::release::artifact::{ArtifactEntry, ArtifactRoot, ReleaseManifest};

    #[test]
    fn rpm_artifact_path_uses_flat_package_version_layout() {
        assert_eq!(
            rpm_artifact_path("0.5.1", "pishoo-0.5.1-1.x86_64.rpm"),
            PathBuf::from("pishoo/0.5.1/pishoo-0.5.1-1.x86_64.rpm")
        );
    }

    #[test]
    fn rpm_filename_parser_extracts_package_and_upstream_version() {
        let (package, version) =
            parse_rpm_filename("pishoo-0.5.1-1.x86_64.rpm").expect("filename should parse");

        assert_eq!(package, "pishoo");
        assert_eq!(version, "0.5.1");
    }

    #[test]
    fn rpm_filename_parser_accepts_common_package() {
        let (package, version) = parse_rpm_filename("pishoo-common-0.5.1-1.noarch.rpm")
            .expect("common filename should parse");

        assert_eq!(package, "pishoo-common");
        assert_eq!(version, "0.5.1");
    }

    #[test]
    fn release_version_rejects_mixed_rpm_versions() {
        let rpms = vec![
            RpmSource {
                package: "pishoo".to_string(),
                version: "0.5.1".to_string(),
                filename: "pishoo-0.5.1-1.x86_64.rpm".to_string(),
                source: PathBuf::from("pishoo-0.5.1-1.x86_64.rpm"),
            },
            RpmSource {
                package: "pishoo".to_string(),
                version: "0.5.2".to_string(),
                filename: "pishoo-0.5.2-1.aarch64.rpm".to_string(),
                source: PathBuf::from("pishoo-0.5.2-1.aarch64.rpm"),
            },
        ];

        let error = release_version(&rpms).expect_err("mixed versions should fail");

        assert!(
            error
                .to_string()
                .starts_with("rpm packages contain multiple release versions")
        );
    }

    #[test]
    fn duplicate_rpm_artifact_paths_are_rejected() {
        let rpms = vec![
            RpmSource {
                package: "pishoo".to_string(),
                version: "0.5.1".to_string(),
                filename: "pishoo-0.5.1-1.x86_64.rpm".to_string(),
                source: PathBuf::from(
                    "x86_64-unknown-linux-gnu/release/rpm/pishoo-0.5.1-1.x86_64.rpm",
                ),
            },
            RpmSource {
                package: "pishoo".to_string(),
                version: "0.5.1".to_string(),
                filename: "pishoo-0.5.1-1.x86_64.rpm".to_string(),
                source: PathBuf::from("common/release/rpm/pishoo-0.5.1-1.x86_64.rpm"),
            },
        ];

        let error =
            validate_unique_artifact_paths(&rpms, "0.5.1").expect_err("duplicates should fail");

        assert!(
            error
                .to_string()
                .starts_with("duplicate rpm artifact path pishoo/0.5.1/pishoo-0.5.1-1.x86_64.rpm")
        );
    }

    #[test]
    fn manifest_merge_preserves_non_rpm_entries_and_replaces_stale_rpm() {
        let existing = ReleaseManifest {
            schema_version: 1,
            package: "pishoo".to_string(),
            version: "old".to_string(),
            artifacts: vec![
                ArtifactEntry {
                    root: ArtifactRoot::Apt,
                    path: "pool/main/g/pishoo/pishoo_0.5.1-1_amd64.deb".to_string(),
                    sha256: "apt-sha".to_string(),
                    immutable: true,
                },
                ArtifactEntry {
                    root: ArtifactRoot::Rpm,
                    path: "stale.rpm".to_string(),
                    sha256: "stale-sha".to_string(),
                    immutable: true,
                },
            ],
        };

        let merged = merge_rpm_manifest(
            existing,
            "0.5.1",
            vec![RpmInfo {
                path: "pishoo/0.5.1/pishoo-0.5.1-1.x86_64.rpm".to_string(),
                sha256: "rpm-sha".to_string(),
            }],
        );

        assert!(
            merged
                .artifacts
                .iter()
                .any(|artifact| artifact.root == ArtifactRoot::Apt
                    && artifact.path == "pool/main/g/pishoo/pishoo_0.5.1-1_amd64.deb")
        );
        assert!(
            !merged
                .artifacts
                .iter()
                .any(|artifact| artifact.path == "stale.rpm")
        );
        assert!(
            merged
                .artifacts
                .iter()
                .any(|artifact| artifact.root == ArtifactRoot::Rpm
                    && artifact.path == "pishoo/0.5.1/pishoo-0.5.1-1.x86_64.rpm"
                    && artifact.immutable)
        );
    }
}
