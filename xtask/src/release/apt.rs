use std::path::{Path, PathBuf};

use bollard::{
    Docker,
    models::{ContainerConfig, ContainerCreateBody, HostConfig, Mount, MountTypeEnum},
    query_parameters::{
        CommitContainerOptionsBuilder, CreateContainerOptionsBuilder, CreateImageOptionsBuilder,
    },
};
use futures_util::StreamExt;
use snafu::{OptionExt, ResultExt, Whatever};
use tempfile::TempDir;
use tracing::info;

use super::{
    AptOptions,
    artifact::{
        ArtifactEntry, ArtifactRoot, ReleaseManifest, copy_artifact, read_manifest, relative_path,
        sha256_file, write_manifest,
    },
    paths::{common_paths, promote_staged_outputs, recreate_dir},
};
use crate::{
    container::{
        check_docker, exec_in_container, force_remove_container, host_uid_gid,
        remove_container_if_exists, start_container,
    },
    target_dir,
};

const PACKAGE_NAME: &str = "pishoo";
const DEB_SEARCH_DIRS: [&str; 5] = [
    "x86_64-unknown-linux-gnu/release/deb",
    "aarch64-unknown-linux-gnu/release/deb",
    "armv7-unknown-linux-gnueabihf/release/deb",
    "i686-unknown-linux-gnu/release/deb",
    "common/deb",
];
const APT_ARCHES: [&str; 4] = ["amd64", "arm64", "armhf", "i386"];
const APT_STAGE_BASE_IMAGE: &str = "debian:bookworm";
const APT_STAGE_IMAGE: &str = "xtask-apt-stage:bookworm-v1";
const APT_REPOSITORY_TARGET: &str = "/apt-repository";
const APT_KEY_TARGET: &str = "/apt-secrets/key.asc";
const APT_PASSPHRASE_TARGET: &str = "/apt-secrets/passphrase";
const APT_GPG_HOME: &str = "/tmp/xtask-apt-gpg";

#[derive(Debug)]
struct DebSource {
    package: String,
    version: String,
    filename: String,
    source: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BinaryMetadataPaths {
    packages: PathBuf,
    packages_gz: PathBuf,
    release: PathBuf,
}

#[derive(Debug)]
struct AptContainerOptions {
    suite: String,
    fingerprint: String,
    has_passphrase_file: bool,
}

struct AptStageContainer {
    docker: Docker,
    container_id: String,
    user: String,
    _secrets: TempDir,
}

impl AptStageContainer {
    async fn start(repository: &Path, options: &AptOptions) -> Result<Self, Whatever> {
        let docker = Docker::connect_with_local_defaults()
            .whatever_context("failed to connect to Docker/Podman")?;
        check_docker(&docker).await?;
        ensure_apt_stage_image(&docker).await?;

        let repository = repository.canonicalize().whatever_context(format!(
            "apt staging path not found: {}",
            repository.display()
        ))?;
        let secrets = tempfile::tempdir()
            .whatever_context("failed to create temporary apt secret directory")?;
        let key_file = secrets.path().join("key.asc");
        write_secret_file(&key_file, &options.signing_key, "signing key").await?;
        let key_file = key_file
            .canonicalize()
            .whatever_context("failed to resolve temporary apt signing key")?;
        let passphrase_file = if let Some(passphrase) = &options.signing_passphrase {
            let path = secrets.path().join("passphrase");
            write_secret_file(&path, passphrase, "signing passphrase").await?;
            Some(
                path.canonicalize()
                    .whatever_context("failed to resolve temporary apt signing passphrase")?,
            )
        } else {
            None
        };

        let container_name = format!("{PACKAGE_NAME}-xtask-apt-stage");
        remove_container_if_exists(&docker, &container_name).await;

        let mut mounts = vec![
            Mount {
                target: Some(APT_REPOSITORY_TARGET.to_string()),
                source: Some(repository.to_string_lossy().into_owned()),
                typ: Some(MountTypeEnum::BIND),
                ..Default::default()
            },
            Mount {
                target: Some(APT_KEY_TARGET.to_string()),
                source: Some(key_file.to_string_lossy().into_owned()),
                typ: Some(MountTypeEnum::BIND),
                read_only: Some(true),
                ..Default::default()
            },
        ];
        if let Some(passphrase_file) = passphrase_file {
            mounts.push(Mount {
                target: Some(APT_PASSPHRASE_TARGET.to_string()),
                source: Some(passphrase_file.to_string_lossy().into_owned()),
                typ: Some(MountTypeEnum::BIND),
                read_only: Some(true),
                ..Default::default()
            });
        }

        let container = docker
            .create_container(
                Some(
                    CreateContainerOptionsBuilder::default()
                        .name(&container_name)
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
            .whatever_context("failed to create apt stage container")?;
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
        info!(tag = APT_STAGE_IMAGE, "apt stage image already exists");
        return Ok(());
    }

    ensure_apt_stage_base_image(docker).await?;

    let container_name = format!("{PACKAGE_NAME}-xtask-apt-stage-setup");
    remove_container_if_exists(docker, &container_name).await;
    let container = docker
        .create_container(
            Some(
                CreateContainerOptionsBuilder::default()
                    .name(&container_name)
                    .build(),
            ),
            ContainerCreateBody {
                image: Some(APT_STAGE_BASE_IMAGE.to_string()),
                cmd: Some(vec!["sleep".into(), "infinity".into()]),
                ..Default::default()
            },
        )
        .await
        .whatever_context("failed to create apt stage setup container")?;

    let setup_result = ensure_apt_stage_image_inner(docker, &container.id).await;
    if setup_result.is_err() {
        force_remove_container(docker, &container.id).await;
        setup_result?;
        unreachable!();
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
        .whatever_context("failed to commit apt stage image");

    force_remove_container(docker, &container.id).await;
    commit_result?;

    info!(tag = APT_STAGE_IMAGE, "apt stage image ready");
    Ok(())
}

async fn ensure_apt_stage_base_image(docker: &Docker) -> Result<(), Whatever> {
    if docker.inspect_image(APT_STAGE_BASE_IMAGE).await.is_ok() {
        info!(
            image = APT_STAGE_BASE_IMAGE,
            "apt stage base image already exists"
        );
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

    Ok(())
}

async fn ensure_apt_stage_image_inner(docker: &Docker, container_id: &str) -> Result<(), Whatever> {
    start_container(docker, container_id).await?;
    exec_in_container(
        docker,
        container_id,
        &[
            "bash",
            "-lc",
            "set -euo pipefail\n\
             export DEBIAN_FRONTEND=noninteractive\n\
             apt-get update -qq\n\
             apt-get install --assume-yes -qq apt-utils ca-certificates dpkg-dev gnupg gzip",
        ],
        None,
    )
    .await
}

pub async fn stage(options: AptOptions) -> Result<(), Whatever> {
    info!(suite = %options.suite, "starting apt repository stage");
    validate_options(&options)?;

    let target_dir = target_dir()?;
    let paths = common_paths()?;
    let debs = discover_debs(&target_dir).await?;
    let version = release_version(&debs)?;
    let manifest = read_existing_manifest(&paths.manifest).await?;
    let staging = paths.root.join("apt.staging");
    recreate_dir(&staging).await?;

    let mut artifact_entries = Vec::new();
    for deb in debs {
        let relative = pool_path(&deb.package, &deb.filename);
        let destination = staging.join(&relative);
        copy_artifact(&deb.source, &destination).await?;
        artifact_entries.push(artifact_entry(&staging, &destination, true).await?);
        info!(path = %destination.display(), "staged deb package");
    }

    let tools = AptStageContainer::start(&staging, &options).await?;
    let result = async {
        let mut metadata_files = generate_binary_metadata(&staging, &options, &tools).await?;
        let release = generate_suite_release(&staging, &options.suite, &tools).await?;
        metadata_files.push(release.clone());
        let signed = sign_suite_release(&staging, &options, &tools).await?;
        metadata_files.extend(signed);
        Ok::<_, Whatever>(metadata_files)
    }
    .await;
    tools.cleanup().await;
    let metadata_files = result?;

    for metadata_file in metadata_files {
        artifact_entries.push(artifact_entry(&staging, &metadata_file, false).await?);
    }

    let manifest = merge_apt_manifest(manifest, &version, artifact_entries);
    let manifest_staging = paths.root.join("manifest.toml.staging");
    write_manifest(&manifest_staging, &manifest).await?;

    promote_staged_outputs(
        "apt",
        &staging,
        &paths.apt,
        &manifest_staging,
        &paths.manifest,
    )
    .await?;

    info!(path = %paths.apt.display(), "finished apt repository stage");
    Ok(())
}

fn validate_options(options: &AptOptions) -> Result<(), Whatever> {
    validate_path_segment("suite", &options.suite)?;
    snafu::ensure_whatever!(
        !options.components.is_empty(),
        "at least one apt component is required"
    );
    for component in &options.components {
        validate_path_segment("component", component)?;
    }
    Ok(())
}

fn validate_path_segment(kind: &str, value: &str) -> Result<(), Whatever> {
    snafu::ensure_whatever!(!value.is_empty(), "{kind} must not be empty");
    snafu::ensure_whatever!(
        !value.contains('/') && !value.contains('\\'),
        "{kind} must be a single path segment"
    );
    snafu::ensure_whatever!(
        value != "." && value != "..",
        "{kind} must be a normal path segment"
    );
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

async fn discover_debs(target_dir: &Path) -> Result<Vec<DebSource>, Whatever> {
    let mut debs = Vec::new();
    for relative in DEB_SEARCH_DIRS {
        let directory = target_dir.join(relative);
        if !tokio::fs::try_exists(&directory)
            .await
            .whatever_context(format!("failed to inspect {}", directory.display()))?
        {
            info!(path = %directory.display(), "skipping missing deb directory");
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
                || path.extension().and_then(|extension| extension.to_str()) != Some("deb")
            {
                continue;
            }
            let filename = path
                .file_name()
                .and_then(|name| name.to_str())
                .whatever_context("failed to read deb filename as utf-8")?
                .to_string();
            let (binary_package, version) = parse_deb_filename(&filename)?;
            let package = pool_package(&binary_package);
            debs.push(DebSource {
                package,
                version,
                filename,
                source: path,
            });
        }
    }

    snafu::ensure_whatever!(
        !debs.is_empty(),
        "no deb packages found in target directories"
    );
    Ok(debs)
}

fn pool_package(binary_package: &str) -> String {
    if binary_package == format!("{PACKAGE_NAME}-common") {
        PACKAGE_NAME.to_string()
    } else {
        binary_package.to_string()
    }
}

fn parse_deb_filename(filename: &str) -> Result<(String, String), Whatever> {
    let (package, rest) = filename
        .split_once('_')
        .whatever_context(format!("failed to infer package name from {filename}"))?;
    let (version_revision, _) = rest
        .split_once('_')
        .whatever_context(format!("failed to infer package version from {filename}"))?;
    let version = version_revision
        .rsplit_once('-')
        .map(|(version, _)| version)
        .unwrap_or(version_revision);
    Ok((package.to_string(), version.to_string()))
}

fn release_version(debs: &[DebSource]) -> Result<String, Whatever> {
    let version = debs
        .first()
        .whatever_context("no deb packages found in target directories")?
        .version
        .clone();
    snafu::ensure_whatever!(
        debs.iter().all(|deb| deb.version == version),
        "deb packages contain multiple release versions"
    );
    Ok(version)
}

async fn generate_binary_metadata(
    repository: &Path,
    options: &AptOptions,
    tools: &AptStageContainer,
) -> Result<Vec<PathBuf>, Whatever> {
    let mut metadata_files = Vec::new();
    for component in &options.components {
        for arch in APT_ARCHES {
            let paths = binary_metadata_paths(&options.suite, component, arch);
            let directory = repository.join(
                paths
                    .packages
                    .parent()
                    .whatever_context("binary package path must have a parent")?,
            );
            tokio::fs::create_dir_all(&directory)
                .await
                .whatever_context(format!("failed to create {}", directory.display()))?;
            let packages = repository.join(&paths.packages);
            if component == "main" {
                scan_packages(arch, &paths.packages, tools).await?;
            } else {
                tokio::fs::write(&packages, "")
                    .await
                    .whatever_context(format!("failed to write {}", packages.display()))?;
            }

            let packages_gz = repository.join(&paths.packages_gz);
            gzip_file(&paths.packages, &paths.packages_gz, tools).await?;
            let release = repository.join(&paths.release);
            write_binary_release(&release, &options.suite, component, arch).await?;
            metadata_files.extend([packages, packages_gz, release]);
        }
    }
    Ok(metadata_files)
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
    arch: &str,
    packages: &Path,
    tools: &AptStageContainer,
) -> Result<(), Whatever> {
    let script = format!(
        "dpkg-scanpackages --arch {} pool /dev/null > {}",
        shell_quote(arch),
        shell_quote_path(packages)
    );
    tools
        .run_in_repository(&script)
        .await
        .whatever_context(format!("failed to generate {}", packages.display()))
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
) -> Result<(), Whatever> {
    let content = format!("Archive: {suite}\nComponent: {component}\nArchitecture: {arch}\n");
    tokio::fs::write(path, content)
        .await
        .whatever_context(format!("failed to write {}", path.display()))
}

async fn generate_suite_release(
    repository: &Path,
    suite: &str,
    tools: &AptStageContainer,
) -> Result<PathBuf, Whatever> {
    let release = repository.join("dists").join(suite).join("Release");
    let relative = PathBuf::from("dists").join(suite).join("Release");
    let suite_dir = PathBuf::from("dists").join(suite);
    let script = format!(
        "apt-ftparchive release {} > {}",
        shell_quote_path(&suite_dir),
        shell_quote_path(&relative)
    );
    tools
        .run_in_repository(&script)
        .await
        .whatever_context(format!("failed to generate {}", release.display()))?;
    Ok(release)
}

async fn sign_suite_release(
    repository: &Path,
    options: &AptOptions,
    tools: &AptStageContainer,
) -> Result<Vec<PathBuf>, Whatever> {
    let release = repository
        .join("dists")
        .join(&options.suite)
        .join("Release");
    let release_gpg = repository
        .join("dists")
        .join(&options.suite)
        .join("Release.gpg");
    let in_release = repository
        .join("dists")
        .join(&options.suite)
        .join("InRelease");

    let script = sign_suite_release_script(&AptContainerOptions {
        suite: options.suite.clone(),
        fingerprint: options.fingerprint.clone(),
        has_passphrase_file: options.signing_passphrase.is_some(),
    });
    tools
        .run_in_repository(&script)
        .await
        .whatever_context(format!("failed to sign {}", release.display()))?;

    Ok(vec![release_gpg, in_release])
}

#[cfg(test)]
fn fingerprint_matches(actual: &str, expected: &str) -> bool {
    !expected.is_empty() && actual == expected
}

fn normalize_fingerprint(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_whitespace())
        .flat_map(char::to_uppercase)
        .collect()
}

fn sign_suite_release_script(options: &AptContainerOptions) -> String {
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

fn shell_quote_path(path: &Path) -> String {
    shell_quote(&path.to_string_lossy())
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

async fn artifact_entry(
    root: &Path,
    file: &Path,
    immutable: bool,
) -> Result<ArtifactEntry, Whatever> {
    Ok(ArtifactEntry {
        root: ArtifactRoot::Apt,
        path: relative_path(root, file)?,
        sha256: sha256_file(file).await?,
        immutable,
    })
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

fn merge_apt_manifest(
    mut manifest: ReleaseManifest,
    version: &str,
    artifacts: Vec<ArtifactEntry>,
) -> ReleaseManifest {
    manifest.package = PACKAGE_NAME.to_string();
    manifest.version = version.to_string();
    manifest
        .artifacts
        .retain(|artifact| artifact.root != ArtifactRoot::Apt);
    manifest.artifacts.extend(artifacts);
    manifest
}

#[cfg(test)]
mod tests {
    use super::{
        AptContainerOptions, binary_metadata_paths, fingerprint_matches, normalize_fingerprint,
        parse_deb_filename, pool_package, pool_path, sign_suite_release_script,
        validate_path_segment,
    };

    #[test]
    fn pool_path_uses_debian_pool_layout() {
        assert_eq!(
            pool_path("pishoo", "pishoo_0.5.1-1_amd64.deb"),
            std::path::PathBuf::from("pool/main/p/pishoo/pishoo_0.5.1-1_amd64.deb")
        );
    }

    #[test]
    fn common_package_pool_path_uses_source_package() {
        assert_eq!(pool_package("pishoo-common"), "pishoo");
        assert_eq!(
            pool_path("pishoo", "pishoo-common_0.5.1-1_all.deb"),
            std::path::PathBuf::from("pool/main/p/pishoo/pishoo-common_0.5.1-1_all.deb")
        );
    }

    #[test]
    fn binary_metadata_paths_use_apt_layout() {
        let paths = binary_metadata_paths("stable", "main", "amd64");

        assert_eq!(
            paths.packages,
            std::path::PathBuf::from("dists/stable/main/binary-amd64/Packages")
        );
        assert_eq!(
            paths.packages_gz,
            std::path::PathBuf::from("dists/stable/main/binary-amd64/Packages.gz")
        );
        assert_eq!(
            paths.release,
            std::path::PathBuf::from("dists/stable/main/binary-amd64/Release")
        );
    }

    #[test]
    fn deb_filename_parser_extracts_package_and_upstream_version() {
        let (package, version) =
            parse_deb_filename("pishoo_0.5.1-1_amd64.deb").expect("filename should parse");

        assert_eq!(package, "pishoo");
        assert_eq!(version, "0.5.1");
    }

    #[test]
    fn path_segment_rejects_path_traversal() {
        let error = validate_path_segment("suite", "../evil")
            .expect_err("path segment with slash should fail");

        assert!(
            error
                .to_string()
                .starts_with("suite must be a single path segment")
        );
    }

    #[test]
    fn fingerprint_normalization_removes_spaces_and_uppercases() {
        assert_eq!(normalize_fingerprint("ab cd ef"), "ABCDEF");
    }

    #[test]
    fn fingerprint_matching_requires_full_fingerprint() {
        assert!(fingerprint_matches(
            "00112233445566778899AABBCCDDEEFF00112233",
            "00112233445566778899AABBCCDDEEFF00112233"
        ));
        assert!(!fingerprint_matches(
            "00112233445566778899AABBCCDDEEFF00112233",
            "CCDDEEFF00112233"
        ));
    }

    #[test]
    fn sign_script_uses_container_secret_paths() {
        let script = sign_suite_release_script(&AptContainerOptions {
            suite: "stable".to_string(),
            fingerprint: "00 11 22 33 44 55 66 77 88 99 AA BB CC DD EE FF 00 11 22 33".to_string(),
            has_passphrase_file: true,
        });

        assert!(script.contains("--import '/apt-secrets/key.asc'"));
        assert!(script.contains("--passphrase-file '/apt-secrets/passphrase'"));
        assert!(script.contains("'dists/stable/Release.gpg'"));
        assert!(script.contains("'dists/stable/InRelease'"));
        assert!(!script.contains("apt-ftparchive"));
    }
}
