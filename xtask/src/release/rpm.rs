use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use snafu::{OptionExt, ResultExt, Whatever};
use tracing::info;

use super::{
    artifact::{
        ArtifactEntry, ArtifactRoot, ReleaseManifest, copy_artifact, read_manifest, sha256_file,
        write_manifest,
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

#[derive(Debug, Clone)]
struct PlannedRpm {
    source: PathBuf,
    info: RpmInfo,
}

pub async fn stage() -> Result<(), Whatever> {
    info!("starting rpm stage");

    let target_dir = target_dir()?;
    let paths = common_paths()?;
    let rpms = discover_rpms(&target_dir).await?;
    let version = release_version(&rpms)?;
    let planned_rpms = plan_rpm_artifacts(&rpms, &version).await?;
    let manifest = read_existing_manifest(&paths.manifest).await?;
    let staging = paths.root.join("rpm.staging");
    recreate_dir(&staging).await?;

    for rpm in &planned_rpms {
        let destination = staging.join(&rpm.info.path);
        copy_artifact(&rpm.source, &destination).await?;
        info!(path = %destination.display(), "staged rpm package");
    }

    let rpm_infos = planned_rpms.into_iter().map(|rpm| rpm.info).collect();
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

async fn plan_rpm_artifacts(
    rpms: &[RpmSource],
    version: &str,
) -> Result<Vec<PlannedRpm>, Whatever> {
    let mut seen = HashMap::new();
    let mut planned = Vec::new();

    for rpm in rpms {
        let relative = rpm_artifact_path(version, &rpm.filename);
        let path = artifact_path_string(&relative)?;
        let sha256 = sha256_file(&rpm.source).await?;

        if let Some(&index) = seen.get(&path) {
            let existing: &PlannedRpm = &planned[index];
            if rpm.package == COMMON_PACKAGE_NAME && existing.info.sha256 == sha256 {
                continue;
            }
            if rpm.package == COMMON_PACKAGE_NAME {
                snafu::whatever!(
                    "duplicate pishoo-common rpm artifact {path} has different sha256"
                );
            }
            snafu::whatever!("duplicate rpm artifact path {path}");
        }

        seen.insert(path.clone(), planned.len());
        planned.push(PlannedRpm {
            source: rpm.source.clone(),
            info: RpmInfo { path, sha256 },
        });
    }

    Ok(planned)
}

fn artifact_path_string(path: &Path) -> Result<String, Whatever> {
    path.components()
        .map(|component| {
            component
                .as_os_str()
                .to_str()
                .whatever_context("failed to convert rpm artifact path component to utf-8")
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|components| components.join("/"))
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
        RpmInfo, RpmSource, merge_rpm_manifest, parse_rpm_filename, plan_rpm_artifacts,
        release_version, rpm_artifact_path,
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

    #[tokio::test]
    async fn duplicate_identical_common_rpm_artifacts_are_deduped() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let first = temp.path().join("x86_64/pishoo-common-0.5.1-1.noarch.rpm");
        let second = temp.path().join("aarch64/pishoo-common-0.5.1-1.noarch.rpm");
        std::fs::create_dir_all(first.parent().expect("first should have parent"))
            .expect("first parent should be created");
        std::fs::create_dir_all(second.parent().expect("second should have parent"))
            .expect("second parent should be created");
        std::fs::write(&first, "common rpm").expect("first rpm should be written");
        std::fs::write(&second, "common rpm").expect("second rpm should be written");
        let rpms = vec![
            RpmSource {
                package: "pishoo-common".to_string(),
                version: "0.5.1".to_string(),
                filename: "pishoo-common-0.5.1-1.noarch.rpm".to_string(),
                source: first,
            },
            RpmSource {
                package: "pishoo-common".to_string(),
                version: "0.5.1".to_string(),
                filename: "pishoo-common-0.5.1-1.noarch.rpm".to_string(),
                source: second,
            },
        ];

        let planned = plan_rpm_artifacts(&rpms, "0.5.1")
            .await
            .expect("identical common rpms should dedupe");
        let merged = merge_rpm_manifest(
            ReleaseManifest {
                schema_version: 1,
                package: "pishoo".to_string(),
                version: String::new(),
                artifacts: Vec::new(),
            },
            "0.5.1",
            planned
                .iter()
                .map(|artifact| artifact.info.clone())
                .collect(),
        );

        assert_eq!(planned.len(), 1);
        assert_eq!(
            planned[0].info.path,
            "pishoo/0.5.1/pishoo-common-0.5.1-1.noarch.rpm"
        );
        assert_eq!(merged.artifacts.len(), 1);
    }

    #[tokio::test]
    async fn duplicate_differing_common_rpm_artifacts_fail() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let first = temp.path().join("x86_64/pishoo-common-0.5.1-1.noarch.rpm");
        let second = temp.path().join("aarch64/pishoo-common-0.5.1-1.noarch.rpm");
        std::fs::create_dir_all(first.parent().expect("first should have parent"))
            .expect("first parent should be created");
        std::fs::create_dir_all(second.parent().expect("second should have parent"))
            .expect("second parent should be created");
        std::fs::write(&first, "common rpm one").expect("first rpm should be written");
        std::fs::write(&second, "common rpm two").expect("second rpm should be written");
        let rpms = vec![
            RpmSource {
                package: "pishoo-common".to_string(),
                version: "0.5.1".to_string(),
                filename: "pishoo-common-0.5.1-1.noarch.rpm".to_string(),
                source: first,
            },
            RpmSource {
                package: "pishoo-common".to_string(),
                version: "0.5.1".to_string(),
                filename: "pishoo-common-0.5.1-1.noarch.rpm".to_string(),
                source: second,
            },
        ];

        let error = plan_rpm_artifacts(&rpms, "0.5.1")
            .await
            .expect_err("differing common rpms should fail");

        assert_eq!(
            error.to_string(),
            "duplicate pishoo-common rpm artifact pishoo/0.5.1/pishoo-common-0.5.1-1.noarch.rpm has different sha256"
        );
    }

    #[tokio::test]
    async fn duplicate_identical_arch_specific_rpm_artifacts_still_fail() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let first = temp.path().join("x86_64-a/pishoo-0.5.1-1.x86_64.rpm");
        let second = temp.path().join("x86_64-b/pishoo-0.5.1-1.x86_64.rpm");
        std::fs::create_dir_all(first.parent().expect("first should have parent"))
            .expect("first parent should be created");
        std::fs::create_dir_all(second.parent().expect("second should have parent"))
            .expect("second parent should be created");
        std::fs::write(&first, "same arch rpm").expect("first rpm should be written");
        std::fs::write(&second, "same arch rpm").expect("second rpm should be written");
        let rpms = vec![
            RpmSource {
                package: "pishoo".to_string(),
                version: "0.5.1".to_string(),
                filename: "pishoo-0.5.1-1.x86_64.rpm".to_string(),
                source: first,
            },
            RpmSource {
                package: "pishoo".to_string(),
                version: "0.5.1".to_string(),
                filename: "pishoo-0.5.1-1.x86_64.rpm".to_string(),
                source: second,
            },
        ];

        let error = plan_rpm_artifacts(&rpms, "0.5.1")
            .await
            .expect_err("arch-specific duplicate should fail");

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
