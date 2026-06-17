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

use super::{
    DebPublishTarget, S3Options,
    plan::{PlannedUpload, UploadCondition},
};
use crate::{
    container::{
        check_docker, exec_in_container, force_remove_container, host_uid_gid,
        remove_container_if_exists, start_container,
    },
    package::manifest::{ArtifactKind, PackageArtifact},
    version_cmp::compare_deb_versions,
};

const APT_ARCHES: &[&str] = &["amd64", "arm64", "armhf", "i386"];
const APT_COMPONENTS: &[&str] = &["main", "contrib", "non-free"];
const APT_STAGE_BASE_IMAGE: &str = "debian:bookworm";
const APT_STAGE_IMAGE: &str = "xtask-apt-publish:bookworm-v1";
const APT_REPOSITORY_TARGET: &str = "/apt-repository";
const APT_KEY_TARGET: &str = "/apt-secrets/key.asc";
const APT_PASSPHRASE_TARGET: &str = "/apt-secrets/passphrase";
const APT_GPG_HOME: &str = "/tmp/xtask-apt-gpg";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageEntry {
    pub package: String,
    pub version: String,
    pub architecture: String,
    pub stanza: String,
}

#[derive(Debug)]
pub struct AptContainerOptions {
    pub suite: String,
    pub fingerprint: String,
    pub has_passphrase_file: bool,
}

#[derive(Debug)]
struct AptPublishOptions {
    suite: String,
    fingerprint: String,
    signing_key: String,
    signing_passphrase: Option<String>,
}

#[derive(Debug, Clone)]
struct DebPayload {
    package: String,
    version: String,
    architecture: String,
    filename: String,
    source: PathBuf,
}

#[derive(Debug, Clone)]
struct RemotePackageEntry {
    entry: PackageEntry,
    filename: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BinaryMetadataPaths {
    packages: PathBuf,
    packages_gz: PathBuf,
    release: PathBuf,
}

struct AptStageContainer {
    docker: Docker,
    container_id: String,
    user: String,
    _secrets: TempDir,
}

pub fn apt_upload_order(key: &str) -> u8 {
    if key.contains("/pool/") || key.starts_with("pool/") {
        return 0;
    }
    if key.contains("/binary-") {
        return 1;
    }
    if key.ends_with("InRelease") {
        return 4;
    }
    if key.ends_with("Release.gpg") {
        return 3;
    }
    2
}

pub fn deb_payload_key(prefix: &str, package: &str, filename: &str) -> String {
    let first = package.chars().next().unwrap_or('_');
    let relative = format!("pool/main/{first}/{package}/{filename}");
    format!("{}/{relative}", prefix.trim_matches('/'))
}

pub async fn run(
    options: &S3Options,
    client: &Client,
    target: DebPublishTarget,
) -> Result<(), Whatever> {
    let loaded = super::load_manifest(ArtifactKind::Deb).await?;
    let local_payloads = local_payloads(&loaded.target_dir, &loaded.manifest.artifacts)?;
    let entry_conditions =
        remote_entry_upload_conditions(client, &options.bucket, &target.prefix, &target.suite)
            .await?;
    let remote_entries =
        remote_package_entries(client, &options.bucket, &target.prefix, &target.suite).await?;
    let local_payloads = publishable_local_payloads(local_payloads, &remote_entries)
        .whatever_context("failed to select publishable deb payloads")?;
    let retained_remote = retained_remote_entries(remote_entries);
    let publish_options = AptPublishOptions {
        suite: target.suite.clone(),
        fingerprint: target.fingerprint.clone(),
        signing_key: std::env::var("XTASK_RELEASE_APT_SIGNING_KEY")
            .whatever_context("failed to read XTASK_RELEASE_APT_SIGNING_KEY")?,
        signing_passphrase: std::env::var("XTASK_RELEASE_APT_SIGNING_PASSPHRASE").ok(),
    };
    let repository = build_repository(
        client,
        &options.bucket,
        &target.prefix,
        local_payloads,
        retained_remote,
    )
    .await?;
    generate_repository_metadata(repository.path(), &publish_options).await?;
    let uploads = repository_uploads(repository.path(), &target.prefix)?;
    let mut uploads = super::plan_repository_uploads(client, &options.bucket, uploads).await?;
    apply_entry_upload_conditions(&mut uploads, &entry_conditions)?;
    uploads.sort_by(|left, right| {
        apt_upload_order(&left.key)
            .cmp(&apt_upload_order(&right.key))
            .then_with(|| left.key.cmp(&right.key))
    });

    if options.dry_run {
        for upload in &uploads {
            info!(
                key = %upload.key,
                path = %upload.path.display(),
                suite = %target.suite,
                fingerprint = %target.fingerprint,
                "would upload deb repository artifact"
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
    payloads: &[DebPayload],
    prefix: &super::key::RemotePrefix,
) -> Result<Vec<PlannedUpload>, Whatever> {
    let mut uploads = Vec::new();
    for payload in payloads {
        let actual_sha256 = crate::sha256_file(&payload.source).await?;
        let key = deb_payload_key(prefix.as_str(), &payload.package, &payload.filename);
        let remote = super::remote_artifact_state(client, bucket, &key).await?;
        let Some(condition) = super::plan::plan_immutable_upload(&key, &actual_sha256, remote)
            .whatever_context("remote deb artifact collision")?
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
) -> Result<Vec<DebPayload>, Whatever> {
    artifacts
        .iter()
        .map(|artifact| {
            Ok(DebPayload {
                package: artifact
                    .package_name
                    .clone()
                    .whatever_context("deb package artifact is missing package name")?,
                version: artifact
                    .package_version
                    .clone()
                    .whatever_context("deb package artifact is missing package version")?,
                architecture: artifact
                    .architecture
                    .clone()
                    .whatever_context("deb package artifact is missing architecture")?,
                filename: artifact
                    .archive_name
                    .clone()
                    .whatever_context("deb package artifact is missing archive name")?,
                source: super::artifact_path(target_dir, artifact),
            })
        })
        .collect()
}

fn publishable_local_payloads(
    payloads: Vec<DebPayload>,
    remote_entries: &[RemotePackageEntry],
) -> Result<Vec<DebPayload>, crate::version_cmp::CompareVersionError> {
    let mut latest_remote = BTreeMap::<(String, String), String>::new();
    for entry in remote_entries {
        let key = (
            entry.entry.package.clone(),
            entry.entry.architecture.clone(),
        );
        match latest_remote.get(&key) {
            None => {
                latest_remote.insert(key, entry.entry.version.clone());
            }
            Some(current) => {
                if compare_deb_versions(&entry.entry.version, current)?.is_gt() {
                    latest_remote.insert(key, entry.entry.version.clone());
                }
            }
        }
    }

    let mut selected = Vec::new();
    for payload in payloads {
        let key = (payload.package.clone(), payload.architecture.clone());
        let should_publish = match latest_remote.get(&key) {
            None => true,
            Some(remote_version) => compare_deb_versions(&payload.version, remote_version)?.is_gt(),
        };
        if should_publish {
            selected.push(payload);
        }
    }

    Ok(selected)
}

fn retained_remote_entries(remote_entries: Vec<RemotePackageEntry>) -> Vec<RemotePackageEntry> {
    remote_entries
}

async fn remote_package_entries(
    client: &Client,
    bucket: &str,
    prefix: &super::key::RemotePrefix,
    suite: &str,
) -> Result<Vec<RemotePackageEntry>, Whatever> {
    let mut entries = Vec::new();
    for component in APT_COMPONENTS {
        for arch in APT_ARCHES {
            let key = prefix.join(&format!("dists/{suite}/{component}/binary-{arch}/Packages"));
            let Some(bytes) = super::get_object_bytes(client, bucket, &key).await? else {
                continue;
            };
            let content = String::from_utf8(bytes)
                .whatever_context(format!("remote Packages object {key} was not utf-8"))?;
            entries.extend(parse_remote_packages(&content)?);
        }
    }
    Ok(entries)
}

fn parse_remote_packages(content: &str) -> Result<Vec<RemotePackageEntry>, Whatever> {
    let mut entries = Vec::new();
    for stanza in content.split("\n\n") {
        let stanza = stanza.trim();
        if stanza.is_empty() {
            continue;
        }
        let package = stanza_field(stanza, "Package")
            .whatever_context("remote deb package stanza is missing Package")?;
        let version = stanza_field(stanza, "Version")
            .whatever_context("remote deb package stanza is missing Version")?;
        let architecture = stanza_field(stanza, "Architecture")
            .whatever_context("remote deb package stanza is missing Architecture")?;
        let filename = stanza_field(stanza, "Filename")
            .whatever_context("remote deb package stanza is missing Filename")?;
        entries.push(RemotePackageEntry {
            entry: PackageEntry {
                package,
                version,
                architecture,
                stanza: format!("{stanza}\n"),
            },
            filename,
        });
    }
    Ok(entries)
}

fn stanza_field(stanza: &str, name: &str) -> Option<String> {
    let prefix = format!("{name}:");
    stanza.lines().find_map(|line| {
        line.strip_prefix(&prefix)
            .map(|value| value.trim().to_string())
    })
}

async fn build_repository(
    client: &Client,
    bucket: &str,
    prefix: &super::key::RemotePrefix,
    local_payloads: Vec<DebPayload>,
    retained_remote: Vec<RemotePackageEntry>,
) -> Result<TempDir, Whatever> {
    let repository = tempfile::tempdir().whatever_context("failed to create apt repository")?;
    for payload in local_payloads {
        let relative = pool_path(&payload.package, &payload.filename);
        let destination = repository.path().join(relative);
        copy_file(&payload.source, &destination).await?;
    }
    for remote in retained_remote {
        let destination = repository.path().join(&remote.filename);
        let key = prefix.join(&remote.filename);
        super::download_object(client, bucket, &key, &destination).await?;
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

async fn generate_repository_metadata(
    repository: &Path,
    options: &AptPublishOptions,
) -> Result<(), Whatever> {
    let tools = AptStageContainer::start(repository, options).await?;
    let result = async {
        generate_binary_metadata(&options.suite, &tools).await?;
        generate_suite_release(&options.suite, &tools).await?;
        sign_suite_release(options, &tools).await?;
        Ok::<_, Whatever>(())
    }
    .await;
    tools.cleanup().await;
    result
}

async fn generate_binary_metadata(suite: &str, tools: &AptStageContainer) -> Result<(), Whatever> {
    for component in APT_COMPONENTS {
        for arch in APT_ARCHES {
            let paths = binary_metadata_paths(suite, component, arch);
            let directory = paths
                .packages
                .parent()
                .whatever_context("binary package path must have a parent")?;
            tools
                .run_in_repository(&format!("mkdir -p {}", shell_quote_path(directory)))
                .await?;
            scan_packages(component, arch, &paths.packages, tools).await?;
            gzip_file(&paths.packages, &paths.packages_gz, tools).await?;
            write_binary_release(&paths.release, suite, component, arch, tools).await?;
        }
    }
    Ok(())
}

fn binary_metadata_paths(suite: &str, component: &str, arch: &str) -> BinaryMetadataPaths {
    let base = PathBuf::from("dists")
        .join(suite)
        .join(component)
        .join(format!("binary-{arch}"));
    BinaryMetadataPaths {
        packages: base.join("Packages"),
        packages_gz: base.join("Packages.gz"),
        release: base.join("Release"),
    }
}

async fn scan_packages(
    component: &str,
    arch: &str,
    packages: &Path,
    tools: &AptStageContainer,
) -> Result<(), Whatever> {
    let script = format!(
        "{} > {}",
        scan_packages_script(component, arch, packages),
        shell_quote_path(packages)
    );
    tools
        .run_in_repository(&script)
        .await
        .whatever_context(format!("failed to generate {}", packages.display()))
}

fn scan_packages_script(component: &str, arch: &str, _packages: &Path) -> String {
    let pool = format!("pool/{component}");
    format!(
        "mkdir -p {pool} && dpkg-scanpackages --multiversion --arch {arch} {pool} /dev/null",
        pool = shell_quote(&pool),
        arch = shell_quote(arch),
    )
}

async fn gzip_file(
    source: &Path,
    destination: &Path,
    tools: &AptStageContainer,
) -> Result<(), Whatever> {
    let script = format!(
        "gzip -n -c {} > {}",
        shell_quote_path(source),
        shell_quote_path(destination)
    );
    tools
        .run_in_repository(&script)
        .await
        .whatever_context(format!("failed to compress {}", source.display()))
}

async fn write_binary_release(
    path: &Path,
    suite: &str,
    component: &str,
    arch: &str,
    tools: &AptStageContainer,
) -> Result<(), Whatever> {
    let content = format!("Archive: {suite}\nComponent: {component}\nArchitecture: {arch}\n");
    let script = format!(
        "printf %s {} > {}",
        shell_quote(&content),
        shell_quote_path(path)
    );
    tools
        .run_in_repository(&script)
        .await
        .whatever_context(format!("failed to write {}", path.display()))
}

async fn generate_suite_release(suite: &str, tools: &AptStageContainer) -> Result<(), Whatever> {
    let script = suite_release_script(suite);
    tools
        .run_in_repository(&script)
        .await
        .whatever_context("failed to generate apt suite Release")
}

fn suite_release_script(suite: &str) -> String {
    let relative = PathBuf::from("dists").join(suite).join("Release");
    let suite_dir = PathBuf::from("dists").join(suite);
    let architectures = APT_ARCHES.join(" ");
    let components = APT_COMPONENTS.join(" ");
    let options = [
        ("Origin", "genmeta"),
        ("Label", "genmeta"),
        ("Suite", "stable"),
        ("Codename", suite),
        ("Version", "2025"),
        ("Architectures", &architectures),
        ("Components", &components),
        ("Description", "Genmeta Package Archives"),
    ]
    .into_iter()
    .map(|(name, value)| shell_quote(&format!("APT::FTPArchive::Release::{name}={value}")))
    .map(|argument| format!("-o {argument}"))
    .collect::<Vec<_>>()
    .join(" ");

    format!(
        "apt-ftparchive {options} release {} > {}",
        shell_quote_path(&suite_dir),
        shell_quote_path(&relative)
    )
}

async fn sign_suite_release(
    options: &AptPublishOptions,
    tools: &AptStageContainer,
) -> Result<(), Whatever> {
    let script = sign_suite_release_script(&AptContainerOptions {
        suite: options.suite.clone(),
        fingerprint: options.fingerprint.clone(),
        has_passphrase_file: options.signing_passphrase.is_some(),
    });
    tools
        .run_in_repository(&script)
        .await
        .whatever_context("failed to sign apt suite Release")
}

fn normalize_fingerprint(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_whitespace())
        .flat_map(char::to_uppercase)
        .collect()
}

pub fn sign_suite_release_script(options: &AptContainerOptions) -> String {
    let expected = normalize_fingerprint(&options.fingerprint);
    let release = PathBuf::from("dists").join(&options.suite).join("Release");
    let release_gpg = PathBuf::from("dists")
        .join(&options.suite)
        .join("Release.gpg");
    let in_release = PathBuf::from("dists")
        .join(&options.suite)
        .join("InRelease");
    let passphrase = if options.has_passphrase_file {
        format!(" --passphrase-file {}", shell_quote(APT_PASSPHRASE_TARGET))
    } else {
        String::new()
    };

    format!(
        "rm -rf {gpg_home}\n\
         mkdir -m 700 {gpg_home}\n\
         gpg --batch --homedir {gpg_home} --import {key}\n\
         actual=\"$(gpg --batch --homedir {gpg_home} --with-colons --fingerprint {fingerprint} | awk -F: '$1 == \"fpr\" {{ print toupper($10); exit }}')\"\n\
         if [ \"$actual\" != {fingerprint} ]; then\n\
         \techo 'gpg fingerprint did not match imported key' >&2\n\
         \texit 1\n\
         fi\n\
         gpg --batch --yes --homedir {gpg_home} --pinentry-mode loopback --default-key {fingerprint}{passphrase} --detach-sign --armor -o {release_gpg} {release}\n\
         gpg --batch --yes --homedir {gpg_home} --pinentry-mode loopback --default-key {fingerprint}{passphrase} --clearsign -o {in_release} {release}\n",
        gpg_home = shell_quote(APT_GPG_HOME),
        key = shell_quote(APT_KEY_TARGET),
        fingerprint = shell_quote(&expected),
        passphrase = passphrase,
        release = shell_quote_path(&release),
        release_gpg = shell_quote_path(&release_gpg),
        in_release = shell_quote_path(&in_release),
    )
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
            .whatever_context("failed to make apt repository path relative")?;
        let relative = path_to_slash(relative);
        uploads.push(PlannedUpload {
            path: entry.path().to_path_buf(),
            key: prefix.join(&relative),
            entry: !relative.ends_with(".deb"),
            condition: None,
        });
    }
    Ok(uploads)
}

async fn remote_entry_upload_conditions(
    client: &Client,
    bucket: &str,
    prefix: &super::key::RemotePrefix,
    suite: &str,
) -> Result<BTreeMap<String, UploadCondition>, Whatever> {
    let mut conditions = BTreeMap::new();
    for relative in entry_metadata_relative_paths(suite) {
        let key = prefix.join(&relative);
        let condition = super::remote_upload_condition(client, bucket, &key).await?;
        conditions.insert(key, condition);
    }
    Ok(conditions)
}

fn entry_metadata_relative_paths(suite: &str) -> Vec<String> {
    let mut paths = vec![
        format!("dists/{suite}/Release"),
        format!("dists/{suite}/Release.gpg"),
        format!("dists/{suite}/InRelease"),
    ];
    for component in APT_COMPONENTS {
        for arch in APT_ARCHES {
            let base = format!("dists/{suite}/{component}/binary-{arch}");
            paths.push(format!("{base}/Packages"));
            paths.push(format!("{base}/Packages.gz"));
            paths.push(format!("{base}/Release"));
        }
    }
    paths
}

fn apply_entry_upload_conditions(
    uploads: &mut [PlannedUpload],
    conditions: &BTreeMap<String, UploadCondition>,
) -> Result<(), Whatever> {
    for upload in uploads.iter_mut().filter(|upload| upload.entry) {
        let condition = conditions
            .get(&upload.key)
            .whatever_context(format!(
                "apt metadata upload {} was not captured in remote baseline",
                upload.key
            ))?
            .clone();
        upload.condition = Some(condition);
    }
    Ok(())
}

fn pool_path(package: &str, filename: &str) -> PathBuf {
    let first = package.chars().next().unwrap_or('_');
    PathBuf::from("pool")
        .join("main")
        .join(first.to_string())
        .join(package)
        .join(filename)
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

fn shell_quote_path(path: &Path) -> String {
    shell_quote(&path_to_slash(path))
}

fn shell_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for character in value.chars() {
        if character == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(character);
        }
    }
    quoted.push('\'');
    quoted
}

impl AptStageContainer {
    async fn start(repository: &Path, options: &AptPublishOptions) -> Result<Self, Whatever> {
        let docker = Docker::connect_with_local_defaults()
            .whatever_context("failed to connect to Docker/Podman")?;
        check_docker(&docker).await?;
        ensure_apt_stage_image(&docker).await?;

        let secrets = tempfile::tempdir()
            .whatever_context("failed to create temporary apt secret directory")?;
        let key_file = secrets.path().join("key.asc");
        write_secret_file(&key_file, &options.signing_key, "signing key").await?;
        let passphrase_file = if let Some(passphrase) = &options.signing_passphrase {
            let path = secrets.path().join("passphrase");
            write_secret_file(&path, passphrase, "signing passphrase").await?;
            Some(path)
        } else {
            None
        };

        let mut mounts = vec![
            Mount {
                target: Some(APT_REPOSITORY_TARGET.to_string()),
                source: Some(path_to_mount_source(repository)?),
                typ: Some(MountType::BIND),
                ..Default::default()
            },
            Mount {
                target: Some(APT_KEY_TARGET.to_string()),
                source: Some(path_to_mount_source(&key_file)?),
                typ: Some(MountType::BIND),
                read_only: Some(true),
                ..Default::default()
            },
        ];
        if let Some(passphrase_file) = &passphrase_file {
            mounts.push(Mount {
                target: Some(APT_PASSPHRASE_TARGET.to_string()),
                source: Some(path_to_mount_source(passphrase_file)?),
                typ: Some(MountType::BIND),
                read_only: Some(true),
                ..Default::default()
            });
        }

        let container_name = "pishoo-xtask-apt-publish";
        remove_container_if_exists(&docker, container_name).await;
        let container = docker
            .create_container(
                Some(
                    CreateContainerOptionsBuilder::default()
                        .name(container_name)
                        .build(),
                ),
                ContainerCreateBody {
                    image: Some(APT_STAGE_IMAGE.to_string()),
                    cmd: Some(vec!["sleep".into(), "infinity".into()]),
                    host_config: Some(HostConfig {
                        mounts: Some(mounts),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )
            .await
            .whatever_context("failed to create apt publish container")?;
        start_container(&docker, &container.id).await?;

        Ok(Self {
            docker,
            container_id: container.id,
            user: host_uid_gid()?,
            _secrets: secrets,
        })
    }

    async fn run_in_repository(&self, script: &str) -> Result<(), Whatever> {
        let script = format!("set -euo pipefail\ncd {APT_REPOSITORY_TARGET}\n{script}");
        exec_in_container(
            &self.docker,
            &self.container_id,
            &["bash", "-lc", &script],
            Some(&self.user),
        )
        .await
    }

    async fn cleanup(self) {
        force_remove_container(&self.docker, &self.container_id).await;
    }
}

async fn write_secret_file(path: &Path, value: &str, description: &str) -> Result<(), Whatever> {
    tokio::fs::write(path, value)
        .await
        .whatever_context(format!("failed to write temporary apt {description}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .await
            .whatever_context(format!("failed to protect temporary apt {description}"))?;
    }
    Ok(())
}

async fn ensure_apt_stage_image(docker: &Docker) -> Result<(), Whatever> {
    if docker.inspect_image(APT_STAGE_IMAGE).await.is_ok() {
        return Ok(());
    }
    let mut pull_stream = docker.create_image(
        Some(
            CreateImageOptionsBuilder::default()
                .from_image(APT_STAGE_BASE_IMAGE)
                .build(),
        ),
        None,
        None,
    );
    while let Some(result) = pull_stream.next().await {
        result.whatever_context(format!("failed to pull base image {APT_STAGE_BASE_IMAGE}"))?;
    }
    let container_name = "pishoo-xtask-apt-publish-setup";
    remove_container_if_exists(docker, container_name).await;
    let container = docker
        .create_container(
            Some(
                CreateContainerOptionsBuilder::default()
                    .name(container_name)
                    .build(),
            ),
            ContainerCreateBody {
                image: Some(APT_STAGE_BASE_IMAGE.to_string()),
                cmd: Some(vec!["sleep".into(), "infinity".into()]),
                ..Default::default()
            },
        )
        .await
        .whatever_context("failed to create apt publish setup container")?;
    let result = async {
        start_container(docker, &container.id).await?;
        exec_in_container(
            docker,
            &container.id,
            &[
                "bash",
                "-lc",
                "apt-get update -qq && apt-get install --assume-yes -qq dpkg-dev apt-utils gzip gnupg",
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
    let repo = APT_STAGE_IMAGE.split(':').next().unwrap_or(APT_STAGE_IMAGE);
    let tag = APT_STAGE_IMAGE.split(':').nth(1).unwrap_or("latest");
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
        .whatever_context("failed to commit apt publish image");
    force_remove_container(docker, &container.id).await;
    commit_result?;
    Ok(())
}

fn path_to_mount_source(path: &Path) -> Result<String, Whatever> {
    path.to_str()
        .map(ToOwned::to_owned)
        .whatever_context("mount path must be valid utf-8")
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        path::{Path, PathBuf},
    };

    use super::{
        super::plan::{PlannedUpload, UploadCondition},
        AptContainerOptions, DebPayload, PackageEntry, RemotePackageEntry,
        apply_entry_upload_conditions, apt_upload_order, deb_payload_key,
        entry_metadata_relative_paths, publishable_local_payloads, retained_remote_entries,
        scan_packages_script, sign_suite_release_script, suite_release_script,
    };

    #[test]
    fn manifest_arch_keeps_remote_history_for_same_arch() {
        let remote = vec![
            remote_package_entry("pishoo", "0.5.1-1", "amd64"),
            remote_package_entry("pishoo", "0.5.1-1", "arm64"),
            remote_package_entry("pishoo-common", "0.5.1-1", "all"),
        ];
        let retained_remote = retained_remote_entries(remote);

        assert!(
            retained_remote.iter().any(
                |entry| entry.entry.version == "0.5.1-1" && entry.entry.architecture == "arm64"
            )
        );
        assert!(
            retained_remote.iter().any(
                |entry| entry.entry.version == "0.5.1-1" && entry.entry.architecture == "amd64"
            )
        );
        assert!(retained_remote.iter().any(|entry| {
            entry.entry.package == "pishoo-common"
                && entry.entry.version == "0.5.1-1"
                && entry.entry.architecture == "all"
        }));
    }

    #[test]
    fn scan_packages_script_keeps_multiple_versions() {
        let script = scan_packages_script(
            "main",
            "amd64",
            Path::new("dists/stable/main/binary-amd64/Packages"),
        );

        assert!(script.contains("dpkg-scanpackages --multiversion --arch 'amd64' 'pool/main'"));
    }

    #[test]
    fn apt_metadata_covers_unified_genmeta_components() {
        let paths = entry_metadata_relative_paths("genmeta");

        assert!(paths.contains(&"dists/genmeta/Release".to_string()));
        assert!(paths.contains(&"dists/genmeta/InRelease".to_string()));
        assert!(paths.contains(&"dists/genmeta/Release.gpg".to_string()));
        assert!(paths.contains(&"dists/genmeta/main/binary-amd64/Packages".to_string()));
        assert!(paths.contains(&"dists/genmeta/contrib/binary-amd64/Packages".to_string()));
        assert!(paths.contains(&"dists/genmeta/non-free/binary-amd64/Packages".to_string()));
    }

    #[test]
    fn suite_release_script_describes_genmeta_archive() {
        let script = suite_release_script("genmeta");

        assert!(script.contains("APT::FTPArchive::Release::Origin=genmeta"));
        assert!(script.contains("APT::FTPArchive::Release::Label=genmeta"));
        assert!(script.contains("APT::FTPArchive::Release::Suite=stable"));
        assert!(script.contains("APT::FTPArchive::Release::Codename=genmeta"));
        assert!(script.contains("APT::FTPArchive::Release::Components=main contrib non-free"));
        assert!(script.contains("APT::FTPArchive::Release::Architectures=amd64 arm64 armhf i386"));
        assert!(script.contains("APT::FTPArchive::Release::Description=Genmeta Package Archives"));
    }

    #[test]
    fn entry_upload_conditions_preserve_remote_baseline() {
        let mut uploads = vec![PlannedUpload {
            path: PathBuf::from("/tmp/Packages"),
            key: "ppa/genmeta/dists/genmeta/main/binary-amd64/Packages".to_string(),
            entry: true,
            condition: None,
        }];
        let conditions = BTreeMap::from([(
            "ppa/genmeta/dists/genmeta/main/binary-amd64/Packages".to_string(),
            UploadCondition::IfMatch("\"old-etag\"".to_string()),
        )]);

        apply_entry_upload_conditions(&mut uploads, &conditions)
            .expect("metadata condition should be applied");

        assert_eq!(
            uploads[0].condition,
            Some(UploadCondition::IfMatch("\"old-etag\"".to_string()))
        );
    }

    #[test]
    fn publishable_deb_payloads_skip_equal_and_older_versions() {
        let remote = vec![
            remote_package_entry("pishoo", "0.5.2-1", "amd64"),
            remote_package_entry("pishoo-common", "0.5.2-1", "all"),
        ];
        let local = vec![
            local_payload("pishoo", "0.5.2-1", "amd64"),
            local_payload("pishoo-common", "0.5.1-1", "all"),
        ];

        let publishable =
            publishable_local_payloads(local, &remote).expect("payload filtering should succeed");

        assert!(publishable.is_empty());
    }

    #[test]
    fn publishable_deb_payloads_compare_against_latest_remote_version() {
        let remote = vec![
            remote_package_entry("pishoo", "0.5.1-1", "amd64"),
            remote_package_entry("pishoo", "0.5.2-1", "amd64"),
        ];
        let local = vec![local_payload("pishoo", "0.5.2-2", "amd64")];

        let publishable =
            publishable_local_payloads(local, &remote).expect("payload filtering should succeed");

        assert_eq!(publishable.len(), 1);
        assert_eq!(publishable[0].package, "pishoo");
        assert_eq!(publishable[0].version, "0.5.2-2");
        assert_eq!(publishable[0].architecture, "amd64");
    }

    #[test]
    fn apt_upload_order_places_inrelease_last() {
        let mut keys = [
            "apt/dists/stable/InRelease".to_string(),
            "apt/pool/main/g/pishoo/pishoo.deb".to_string(),
            "apt/dists/stable/Release".to_string(),
        ];
        keys.sort_by_key(|key| apt_upload_order(key));
        assert_eq!(
            keys.last().map(String::as_str),
            Some("apt/dists/stable/InRelease")
        );
    }

    #[test]
    fn apt_upload_order_places_binary_metadata_before_suite_release() {
        let mut keys = [
            "ppa/genmeta/dists/genmeta/InRelease".to_string(),
            "ppa/genmeta/dists/genmeta/Release.gpg".to_string(),
            "ppa/genmeta/dists/genmeta/Release".to_string(),
            "ppa/genmeta/dists/genmeta/main/binary-amd64/Packages".to_string(),
        ];
        keys.sort_by(|left, right| {
            apt_upload_order(left)
                .cmp(&apt_upload_order(right))
                .then_with(|| left.cmp(right))
        });

        assert_eq!(
            keys,
            [
                "ppa/genmeta/dists/genmeta/main/binary-amd64/Packages",
                "ppa/genmeta/dists/genmeta/Release",
                "ppa/genmeta/dists/genmeta/Release.gpg",
                "ppa/genmeta/dists/genmeta/InRelease",
            ]
        );
    }

    #[test]
    fn sign_suite_release_script_checks_fingerprint() {
        let script = sign_suite_release_script(&AptContainerOptions {
            suite: "stable".to_string(),
            fingerprint: "0123456789abcdef".to_string(),
            has_passphrase_file: false,
        });
        assert!(script.contains("gpg fingerprint did not match imported key"));
        assert!(script.contains("'dists/stable/InRelease'"));
    }

    #[test]
    fn deb_payload_key_uses_prefix_and_pool_path() {
        let key = deb_payload_key("apt/pishoo", "pishoo", "pishoo_0.5.2-1_amd64.deb");
        assert_eq!(
            key,
            "apt/pishoo/pool/main/p/pishoo/pishoo_0.5.2-1_amd64.deb"
        );
    }

    fn local_payload(package: &str, version: &str, architecture: &str) -> DebPayload {
        DebPayload {
            package: package.to_string(),
            version: version.to_string(),
            architecture: architecture.to_string(),
            filename: format!("{package}_{version}_{architecture}.deb"),
            source: PathBuf::from(format!("/tmp/{package}_{version}_{architecture}.deb")),
        }
    }

    fn remote_package_entry(
        package: &str,
        version: &str,
        architecture: &str,
    ) -> RemotePackageEntry {
        RemotePackageEntry {
            entry: PackageEntry {
                package: package.to_string(),
                version: version.to_string(),
                architecture: architecture.to_string(),
                stanza: format!(
                    "Package: {package}\nVersion: {version}\nArchitecture: {architecture}\n"
                ),
            },
            filename: format!(
                "pool/main/{}/{package}/{package}_{version}_{architecture}.deb",
                package.chars().next().unwrap_or('_')
            ),
        }
    }
}
