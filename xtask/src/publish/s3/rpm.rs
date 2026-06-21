#![allow(dead_code)]

use std::{
    collections::BTreeMap,
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
use snafu::{OptionExt, ResultExt, Whatever};
use tempfile::TempDir;
use tracing::info;
use walkdir::WalkDir;

use super::{ResolvedS3Options, RpmPublishTarget, plan::PlannedUpload};
use crate::{
    container::{
        check_docker, exec_in_container, force_remove_container, remove_container_if_exists,
        start_container,
    },
    package::manifest::{ArtifactKind, PackageArtifact},
    version_cmp::compare_rpm_versions,
};

const RPM_STAGE_BASE_IMAGE: &str = "fedora:40";
const RPM_STAGE_IMAGE: &str = "xtask-rpm-publish:fedora40-v1";
const RPM_REPOSITORY_TARGET: &str = "/rpm-repository";

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
    options: &ResolvedS3Options,
    client: &Client,
    target: RpmPublishTarget,
) -> Result<(), Whatever> {
    let loaded = super::load_manifest(ArtifactKind::Rpm).await?;
    let local_payloads = local_payloads(&loaded.target_dir, &loaded.manifest.artifacts)?;
    let remote_payloads = remote_rpm_payloads(client, &options.bucket, &target.prefix).await?;
    let local_payloads = publishable_local_payloads(local_payloads, &remote_payloads)
        .whatever_context("failed to select publishable rpm payloads")?;
    let retained_remote = retained_remote_payloads(remote_payloads);
    let repository =
        build_repository(client, &options.bucket, local_payloads, retained_remote).await?;
    generate_repository_metadata(repository.path()).await?;
    let uploads = repository_uploads(repository.path(), &target.prefix)?;
    let mut uploads = super::plan_repository_uploads(client, &options.bucket, uploads).await?;
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
        return Ok(());
    }

    for upload in uploads {
        super::upload_file(
            client,
            &options.bucket,
            &upload.path,
            &upload.key,
            upload.condition,
        )
        .await?;
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
        let Some(condition) = super::plan::plan_immutable_upload(&key, &actual_sha256, remote)
            .whatever_context("remote rpm artifact collision")?
        else {
            continue;
        };
        uploads.push(PlannedUpload {
            path: payload.source.clone(),
            key,
            entry: false,
            condition: Some(condition),
        });
    }
    Ok(uploads)
}

fn local_payloads(
    target_dir: &Path,
    artifacts: &[PackageArtifact],
) -> Result<Vec<RpmPayload>, Whatever> {
    artifacts
        .iter()
        .map(|artifact| {
            Ok(RpmPayload {
                package: artifact
                    .package_name
                    .clone()
                    .whatever_context("rpm package artifact is missing package name")?,
                version: artifact
                    .package_version
                    .clone()
                    .whatever_context("rpm package artifact is missing package version")?,
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

fn publishable_local_payloads(
    payloads: Vec<RpmPayload>,
    remote_payloads: &[RemoteRpmPayload],
) -> Result<Vec<RpmPayload>, crate::version_cmp::CompareVersionError> {
    let mut latest_remote = BTreeMap::<(String, String), String>::new();
    for payload in remote_payloads {
        let key = (payload.package.clone(), payload.architecture.clone());
        match latest_remote.get(&key) {
            None => {
                latest_remote.insert(key, payload.version.clone());
            }
            Some(current) => {
                if compare_rpm_versions(&payload.version, current)?.is_gt() {
                    latest_remote.insert(key, payload.version.clone());
                }
            }
        }
    }

    let mut selected = Vec::new();
    for payload in payloads {
        let key = (payload.package.clone(), payload.architecture.clone());
        let should_publish = match latest_remote.get(&key) {
            None => true,
            Some(remote_version) => compare_rpm_versions(&payload.version, remote_version)?.is_gt(),
        };
        if should_publish {
            selected.push(payload);
        }
    }

    Ok(selected)
}

fn retained_remote_payloads(remote_payloads: Vec<RemoteRpmPayload>) -> Vec<RemoteRpmPayload> {
    remote_payloads
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
            condition: None,
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
    use std::path::{Path, PathBuf};

    use super::{
        RemoteRpmPayload, RpmPayload, local_payloads, publishable_local_payloads,
        retained_remote_payloads, rpm_upload_order,
    };
    use crate::package::manifest::PackageArtifact;

    #[test]
    fn manifest_arch_keeps_remote_history_for_same_arch() {
        let remote = vec![
            remote_payload("pishoo", "0.5.1", "x86_64"),
            remote_payload("pishoo", "0.5.1", "aarch64"),
            remote_payload("pishoo-common", "0.5.1", "noarch"),
        ];

        let retained_remote = retained_remote_payloads(remote);

        assert!(
            retained_remote
                .iter()
                .any(|entry| entry.version == "0.5.1" && entry.architecture == "aarch64")
        );
        assert!(
            retained_remote
                .iter()
                .any(|entry| entry.version == "0.5.1" && entry.architecture == "x86_64")
        );
        assert!(retained_remote.iter().any(|entry| {
            entry.package == "pishoo-common"
                && entry.version == "0.5.1"
                && entry.architecture == "noarch"
        }));
    }

    #[test]
    fn local_rpm_payload_uses_artifact_package_version() {
        let payloads = local_payloads(
            Path::new("/tmp/target"),
            &[artifact(
                "pishoo",
                "0.5.2-3",
                "x86_64",
                "pishoo-0.5.2-3.x86_64.rpm",
            )],
        )
        .expect("payload extraction should succeed");

        assert_eq!(payloads[0].version, "0.5.2-3");
    }

    #[test]
    fn publishable_rpm_payloads_skip_equal_and_older_versions() {
        let remote = vec![
            remote_payload("pishoo", "0.5.2-1", "x86_64"),
            remote_payload("pishoo-common", "0.5.2-1", "noarch"),
        ];
        let local = vec![
            local_payload("pishoo", "0.5.2-1", "x86_64"),
            local_payload("pishoo-common", "0.5.1-1", "noarch"),
        ];

        let publishable =
            publishable_local_payloads(local, &remote).expect("payload filtering should succeed");

        assert!(publishable.is_empty());
    }

    #[test]
    fn publishable_rpm_payloads_compare_against_latest_remote_version() {
        let remote = vec![
            remote_payload("pishoo", "0.5.1-1", "x86_64"),
            remote_payload("pishoo", "0.5.2-1", "x86_64"),
        ];
        let local = vec![local_payload("pishoo", "0.5.2-2", "x86_64")];

        let publishable =
            publishable_local_payloads(local, &remote).expect("payload filtering should succeed");

        assert_eq!(publishable.len(), 1);
        assert_eq!(publishable[0].package, "pishoo");
        assert_eq!(publishable[0].version, "0.5.2-2");
        assert_eq!(publishable[0].architecture, "x86_64");
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

    fn local_payload(package: &str, version: &str, architecture: &str) -> RpmPayload {
        RpmPayload {
            package: package.to_string(),
            version: version.to_string(),
            architecture: architecture.to_string(),
            filename: format!("{package}-{version}-1.{architecture}.rpm"),
            source: PathBuf::from(format!("/tmp/{package}-{version}-1.{architecture}.rpm")),
        }
    }

    fn remote_payload(package: &str, version: &str, architecture: &str) -> RemoteRpmPayload {
        let filename = format!("{package}-{version}-1.{architecture}.rpm");
        RemoteRpmPayload {
            package: package.to_string(),
            version: version.to_string(),
            architecture: architecture.to_string(),
            filename: filename.clone(),
            key: format!("rpm/pishoo/{package}/{version}/{filename}"),
        }
    }

    fn artifact(
        package: &str,
        version: &str,
        architecture: &str,
        archive_name: &str,
    ) -> PackageArtifact {
        PackageArtifact {
            target: architecture.to_string(),
            path: format!("{architecture}/release/rpm/{archive_name}"),
            sha256: "0".repeat(64),
            size: 42,
            package_name: Some(package.to_string()),
            package_version: Some(version.to_string()),
            architecture: Some(architecture.to_string()),
            archive_name: Some(archive_name.to_string()),
            features: Vec::new(),
            profile: Some("release".to_string()),
        }
    }
}
