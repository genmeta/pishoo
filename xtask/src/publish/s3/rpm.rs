#![allow(dead_code)]

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use aws_sdk_s3::Client;
use bollard::{
    Docker,
    models::{ContainerConfig, ContainerCreateBody, HostConfig, Mount, MountType},
    query_parameters::{
        CommitContainerOptionsBuilder, CreateContainerOptionsBuilder, CreateImageOptionsBuilder,
    },
};
use futures_util::StreamExt;
use snafu::{OptionExt, ResultExt, Snafu, Whatever};
use tempfile::TempDir;
use tracing::info;
use walkdir::WalkDir;

use super::{RpmPublishTarget, S3Options, plan::PlannedUpload};
use crate::{
    container::{
        check_docker, exec_in_container, force_remove_container, remove_container_if_exists,
        start_container,
    },
    package::manifest::{ArtifactKind, PackageArtifact},
};

const RPM_STAGE_BASE_IMAGE: &str = "fedora:40";
const RPM_STAGE_IMAGE: &str = "xtask-rpm-publish:fedora40-v1";
const RPM_REPOSITORY_TARGET: &str = "/rpm-repository";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpmEntry {
    pub package: String,
    pub version: String,
    pub architecture: String,
    pub metadata: String,
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum MergeRpmEntriesError {
    #[snafu(display("duplicate local rpm entry for {package} {architecture}"))]
    DuplicateLocal {
        package: String,
        architecture: String,
    },
}

#[derive(Debug, Clone)]
struct RpmPayload {
    package: String,
    version: String,
    architecture: String,
    filename: String,
    source: PathBuf,
}

#[derive(Debug, Clone)]
struct RemoteRpmPayload {
    package: String,
    version: String,
    architecture: String,
    filename: String,
    key: String,
}

struct RpmStageContainer {
    docker: Docker,
    container_id: String,
}

pub fn merge_rpm_entries(
    remote: Vec<RpmEntry>,
    local: Vec<RpmEntry>,
) -> Result<Vec<RpmEntry>, MergeRpmEntriesError> {
    let mut local_keys = BTreeSet::new();
    for entry in &local {
        let key = (entry.package.clone(), entry.architecture.clone());
        snafu::ensure!(
            local_keys.insert(key),
            merge_rpm_entries_error::DuplicateLocalSnafu {
                package: entry.package.clone(),
                architecture: entry.architecture.clone()
            }
        );
    }
    let mut merged = remote
        .into_iter()
        .filter(|entry| !local_keys.contains(&(entry.package.clone(), entry.architecture.clone())))
        .collect::<Vec<_>>();
    merged.extend(local);
    merged.sort_by(|left, right| {
        left.package
            .cmp(&right.package)
            .then_with(|| left.architecture.cmp(&right.architecture))
            .then_with(|| left.version.cmp(&right.version))
    });
    Ok(merged)
}

pub fn rpm_upload_order(key: &str) -> u8 {
    if key.ends_with(".rpm") {
        return 0;
    }
    if key.ends_with("repodata/repomd.xml") {
        return 4;
    }
    2
}

pub fn rpm_payload_key(prefix: &str, package: &str, version: &str, filename: &str) -> String {
    format!(
        "{}/{package}/{version}/{filename}",
        prefix.trim_matches('/')
    )
}

pub async fn run(
    options: &S3Options,
    client: &Client,
    target: RpmPublishTarget,
) -> Result<(), Whatever> {
    let loaded = super::load_manifest(ArtifactKind::Rpm).await?;
    let local_payloads = local_payloads(
        &loaded.target_dir,
        &loaded.manifest.artifacts,
        &loaded.manifest.version,
    )?;
    let mut uploads =
        plan_payload_uploads(client, &options.bucket, &local_payloads, &target.prefix).await?;
    let remote_payloads = remote_rpm_payloads(client, &options.bucket, &target.prefix).await?;
    let local_keys = local_payloads
        .iter()
        .map(|payload| (payload.package.clone(), payload.architecture.clone()))
        .collect::<BTreeSet<_>>();
    let retained_remote = remote_payloads
        .into_iter()
        .filter(|payload| {
            !local_keys.contains(&(payload.package.clone(), payload.architecture.clone()))
        })
        .collect::<Vec<_>>();
    uploads.sort_by(|left, right| {
        rpm_upload_order(&left.key)
            .cmp(&rpm_upload_order(&right.key))
            .then_with(|| left.key.cmp(&right.key))
    });

    if options.dry_run {
        for upload in &uploads {
            info!(
                key = %upload.key,
                path = %upload.path.display(),
                "would upload rpm repository artifact"
            );
        }
        info!(
            retained_remote_count = retained_remote.len(),
            "would retain remote rpm package artifacts"
        );
        return Ok(());
    }

    let repository =
        build_repository(client, &options.bucket, local_payloads, retained_remote).await?;
    generate_repository_metadata(repository.path()).await?;
    let mut uploads = repository_uploads(repository.path(), &target.prefix)?;
    uploads.sort_by(|left, right| {
        rpm_upload_order(&left.key)
            .cmp(&rpm_upload_order(&right.key))
            .then_with(|| left.key.cmp(&right.key))
    });
    for upload in uploads {
        super::upload_file(client, &options.bucket, &upload.path, &upload.key).await?;
    }
    Ok(())
}

async fn plan_payload_uploads(
    client: &Client,
    bucket: &str,
    payloads: &[RpmPayload],
    prefix: &super::key::RemotePrefix,
) -> Result<Vec<PlannedUpload>, Whatever> {
    let mut uploads = Vec::new();
    for payload in payloads {
        let actual_sha256 = crate::sha256_file(&payload.source).await?;
        let key = rpm_payload_key(
            prefix.as_str(),
            &payload.package,
            &payload.version,
            &payload.filename,
        );
        let remote = super::remote_artifact_state(client, bucket, &key).await?;
        super::plan::verify_immutable_collision(&key, &actual_sha256, remote)
            .whatever_context("remote rpm artifact collision")?;
        uploads.push(PlannedUpload {
            path: payload.source.clone(),
            key,
            entry: false,
        });
    }
    Ok(uploads)
}

fn local_payloads(
    target_dir: &Path,
    artifacts: &[PackageArtifact],
    manifest_version: &str,
) -> Result<Vec<RpmPayload>, Whatever> {
    artifacts
        .iter()
        .map(|artifact| {
            Ok(RpmPayload {
                package: artifact
                    .package_name
                    .clone()
                    .whatever_context("rpm package artifact is missing package name")?,
                version: manifest_version.to_string(),
                architecture: artifact
                    .architecture
                    .clone()
                    .whatever_context("rpm package artifact is missing architecture")?,
                filename: artifact
                    .archive_name
                    .clone()
                    .whatever_context("rpm package artifact is missing archive name")?,
                source: super::artifact_path(target_dir, artifact),
            })
        })
        .collect()
}

async fn remote_rpm_payloads(
    client: &Client,
    bucket: &str,
    prefix: &super::key::RemotePrefix,
) -> Result<Vec<RemoteRpmPayload>, Whatever> {
    let keys = super::list_object_keys(client, bucket, prefix.as_str()).await?;
    keys.into_iter()
        .filter(|key| key.ends_with(".rpm"))
        .map(|key| remote_rpm_payload_from_key(prefix.as_str(), key))
        .collect()
}

fn remote_rpm_payload_from_key(prefix: &str, key: String) -> Result<RemoteRpmPayload, Whatever> {
    let relative = key
        .strip_prefix(prefix)
        .and_then(|value| value.strip_prefix('/'))
        .unwrap_or(&key);
    let parts = relative.split('/').collect::<Vec<_>>();
    snafu::ensure_whatever!(
        parts.len() >= 3,
        "remote rpm payload key has unexpected layout {key}"
    );
    let package = parts[0].to_string();
    let version = parts[1].to_string();
    let filename = parts[parts.len() - 1].to_string();
    let architecture = filename
        .strip_suffix(".rpm")
        .and_then(|stem| stem.rsplit_once('.').map(|(_, arch)| arch.to_string()))
        .whatever_context(format!("failed to infer rpm architecture from {filename}"))?;
    Ok(RemoteRpmPayload {
        package,
        version,
        architecture,
        filename,
        key,
    })
}

async fn build_repository(
    client: &Client,
    bucket: &str,
    local_payloads: Vec<RpmPayload>,
    retained_remote: Vec<RemoteRpmPayload>,
) -> Result<TempDir, Whatever> {
    let repository = tempfile::tempdir().whatever_context("failed to create rpm repository")?;
    for payload in local_payloads {
        let destination = repository
            .path()
            .join(&payload.package)
            .join(&payload.version)
            .join(&payload.filename);
        copy_file(&payload.source, &destination).await?;
    }
    for payload in retained_remote {
        let destination = repository
            .path()
            .join(&payload.package)
            .join(&payload.version)
            .join(&payload.filename);
        super::download_object(client, bucket, &payload.key, &destination).await?;
    }
    Ok(repository)
}

async fn copy_file(source: &Path, destination: &Path) -> Result<(), Whatever> {
    if let Some(parent) = destination.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .whatever_context(format!("failed to create {}", parent.display()))?;
    }
    tokio::fs::copy(source, destination)
        .await
        .whatever_context(format!(
            "failed to copy {} to {}",
            source.display(),
            destination.display()
        ))?;
    Ok(())
}

async fn generate_repository_metadata(repository: &Path) -> Result<(), Whatever> {
    let tools = RpmStageContainer::start(repository).await?;
    let result = tools.run_in_repository("createrepo_c .").await;
    tools.cleanup().await;
    result
}

fn repository_uploads(
    repository: &Path,
    prefix: &super::key::RemotePrefix,
) -> Result<Vec<PlannedUpload>, Whatever> {
    let mut uploads = Vec::new();
    for entry in WalkDir::new(repository) {
        let entry = entry.whatever_context(format!("failed to walk {}", repository.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(repository)
            .whatever_context("failed to make rpm repository path relative")?;
        let relative = path_to_slash(relative);
        uploads.push(PlannedUpload {
            path: entry.path().to_path_buf(),
            key: prefix.join(&relative),
            entry: !relative.ends_with(".rpm"),
        });
    }
    Ok(uploads)
}

impl RpmStageContainer {
    async fn start(repository: &Path) -> Result<Self, Whatever> {
        let docker = Docker::connect_with_local_defaults()
            .whatever_context("failed to connect to Docker/Podman")?;
        check_docker(&docker).await?;
        ensure_rpm_stage_image(&docker).await?;
        let container_name = "pishoo-xtask-rpm-publish";
        remove_container_if_exists(&docker, container_name).await;
        let container = docker
            .create_container(
                Some(
                    CreateContainerOptionsBuilder::default()
                        .name(container_name)
                        .build(),
                ),
                ContainerCreateBody {
                    image: Some(RPM_STAGE_IMAGE.to_string()),
                    cmd: Some(vec!["sleep".into(), "infinity".into()]),
                    host_config: Some(HostConfig {
                        mounts: Some(vec![Mount {
                            target: Some(RPM_REPOSITORY_TARGET.to_string()),
                            source: Some(path_to_mount_source(repository)?),
                            typ: Some(MountType::BIND),
                            ..Default::default()
                        }]),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )
            .await
            .whatever_context("failed to create rpm publish container")?;
        start_container(&docker, &container.id).await?;
        Ok(Self {
            docker,
            container_id: container.id,
        })
    }

    async fn run_in_repository(&self, script: &str) -> Result<(), Whatever> {
        let script = format!("set -euo pipefail\ncd {RPM_REPOSITORY_TARGET}\n{script}");
        exec_in_container(
            &self.docker,
            &self.container_id,
            &["bash", "-lc", &script],
            None,
        )
        .await
    }

    async fn cleanup(self) {
        force_remove_container(&self.docker, &self.container_id).await;
    }
}

async fn ensure_rpm_stage_image(docker: &Docker) -> Result<(), Whatever> {
    if docker.inspect_image(RPM_STAGE_IMAGE).await.is_ok() {
        return Ok(());
    }
    let mut pull_stream = docker.create_image(
        Some(
            CreateImageOptionsBuilder::default()
                .from_image(RPM_STAGE_BASE_IMAGE)
                .build(),
        ),
        None,
        None,
    );
    while let Some(result) = pull_stream.next().await {
        result.whatever_context(format!("failed to pull base image {RPM_STAGE_BASE_IMAGE}"))?;
    }
    let container_name = "pishoo-xtask-rpm-publish-setup";
    remove_container_if_exists(docker, container_name).await;
    let container = docker
        .create_container(
            Some(
                CreateContainerOptionsBuilder::default()
                    .name(container_name)
                    .build(),
            ),
            ContainerCreateBody {
                image: Some(RPM_STAGE_BASE_IMAGE.to_string()),
                cmd: Some(vec!["sleep".into(), "infinity".into()]),
                ..Default::default()
            },
        )
        .await
        .whatever_context("failed to create rpm publish setup container")?;
    let result = async {
        start_container(docker, &container.id).await?;
        exec_in_container(
            docker,
            &container.id,
            &[
                "bash",
                "-lc",
                "dnf install -y --setopt=install_weak_deps=False createrepo_c",
            ],
            None,
        )
        .await
    }
    .await;
    if result.is_err() {
        force_remove_container(docker, &container.id).await;
        result?;
    }
    let repo = RPM_STAGE_IMAGE.split(':').next().unwrap_or(RPM_STAGE_IMAGE);
    let tag = RPM_STAGE_IMAGE.split(':').nth(1).unwrap_or("latest");
    let commit_result = docker
        .commit_container(
            CommitContainerOptionsBuilder::default()
                .container(&container.id)
                .repo(repo)
                .tag(tag)
                .build(),
            ContainerConfig::default(),
        )
        .await
        .whatever_context("failed to commit rpm publish image");
    force_remove_container(docker, &container.id).await;
    commit_result?;
    Ok(())
}

fn path_to_slash(path: &Path) -> String {
    path.components()
        .map(|component| {
            component
                .as_os_str()
                .to_str()
                .expect("repository path component must be valid utf-8")
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn path_to_mount_source(path: &Path) -> Result<String, Whatever> {
    path.to_str()
        .map(ToOwned::to_owned)
        .whatever_context("mount path must be valid utf-8")
}

#[cfg(test)]
mod tests {
    use super::{RpmEntry, merge_rpm_entries, rpm_upload_order};

    #[test]
    fn manifest_arch_replaces_remote_same_arch_and_preserves_others() {
        let remote = vec![
            RpmEntry {
                package: "pishoo".to_string(),
                version: "0.5.1-1".to_string(),
                architecture: "x86_64".to_string(),
                metadata: "name=pishoo version=0.5.1-1 arch=x86_64".to_string(),
            },
            RpmEntry {
                package: "pishoo".to_string(),
                version: "0.5.1-1".to_string(),
                architecture: "aarch64".to_string(),
                metadata: "name=pishoo version=0.5.1-1 arch=aarch64".to_string(),
            },
            RpmEntry {
                package: "pishoo-common".to_string(),
                version: "0.5.1-1".to_string(),
                architecture: "noarch".to_string(),
                metadata: "name=pishoo-common version=0.5.1-1 arch=noarch".to_string(),
            },
        ];
        let local = vec![
            RpmEntry {
                package: "pishoo".to_string(),
                version: "0.5.2-1".to_string(),
                architecture: "x86_64".to_string(),
                metadata: "name=pishoo version=0.5.2-1 arch=x86_64".to_string(),
            },
            RpmEntry {
                package: "pishoo-common".to_string(),
                version: "0.5.2-1".to_string(),
                architecture: "noarch".to_string(),
                metadata: "name=pishoo-common version=0.5.2-1 arch=noarch".to_string(),
            },
        ];

        let merged = merge_rpm_entries(remote, local).expect("merge should pass");

        assert!(
            merged
                .iter()
                .any(|entry| entry.version == "0.5.2-1" && entry.architecture == "x86_64")
        );
        assert!(
            merged
                .iter()
                .any(|entry| entry.version == "0.5.1-1" && entry.architecture == "aarch64")
        );
        assert!(merged.iter().any(|entry| {
            entry.package == "pishoo-common"
                && entry.version == "0.5.2-1"
                && entry.architecture == "noarch"
        }));
    }

    #[test]
    fn rpm_upload_order_places_repomd_last() {
        let mut keys = [
            "rpm/repodata/repomd.xml".to_string(),
            "rpm/pishoo/0.5.2/pishoo.rpm".to_string(),
            "rpm/repodata/primary.xml.gz".to_string(),
        ];
        keys.sort_by_key(|key| rpm_upload_order(key));
        assert_eq!(
            keys.last().map(String::as_str),
            Some("rpm/repodata/repomd.xml")
        );
    }

    #[test]
    fn rpm_payload_key_uses_package_version_layout() {
        let key =
            super::rpm_payload_key("rpm/pishoo", "pishoo", "0.5.2", "pishoo-0.5.2-1.x86_64.rpm");
        assert_eq!(key, "rpm/pishoo/pishoo/0.5.2/pishoo-0.5.2-1.x86_64.rpm");
    }
}
