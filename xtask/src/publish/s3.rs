use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use aws_credential_types::Credentials;
use aws_sdk_s3::{
    Client,
    config::Region,
    error::SdkError,
    operation::{get_object::GetObjectError, put_object::PutObjectError},
    primitives::ByteStream,
};
use clap::{Args, CommandFactory, Parser, Subcommand, error::ErrorKind};
use sha2::Digest;
use snafu::{OptionExt, ResultExt, Whatever};
use tracing::info;

use crate::{
    grouped,
    package::manifest::{ArtifactKind, PackageManifest, validate_manifest},
};

pub mod brew;
pub mod deb;
pub mod key;
pub mod plan;
pub mod rpm;

#[derive(Debug, Clone, Args)]
pub struct S3Options {
    /// S3 endpoint URL
    #[arg(long)]
    pub endpoint_url: String,
    /// S3 bucket name
    #[arg(long)]
    pub bucket: String,
    /// AWS access key id
    #[arg(long, env = "XTASK_RELEASE_S3_ACCESS_KEY_ID", hide_env_values = true)]
    pub access_key_id: String,
    /// AWS secret access key
    #[arg(
        long,
        env = "XTASK_RELEASE_S3_SECRET_ACCESS_KEY",
        hide_env_values = true
    )]
    pub secret_access_key: String,
    /// Print the publish plan without uploading
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct BrewPublishTarget {
    pub prefix: key::RemotePrefix,
    pub public_base_url: key::PublicBaseUrl,
}

#[derive(Debug, Clone)]
pub(crate) struct DebPublishTarget {
    pub prefix: key::RemotePrefix,
    pub suite: String,
    pub fingerprint: String,
}

#[derive(Debug, Clone)]
pub(crate) struct RpmPublishTarget {
    pub prefix: key::RemotePrefix,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) enum S3Target {
    Brew(BrewPublishTarget),
    Deb(DebPublishTarget),
    Rpm(RpmPublishTarget),
}

#[derive(Debug, Parser)]
struct S3TargetCli {
    #[command(subcommand)]
    target: S3TargetFormat,
}

#[derive(Debug, Subcommand)]
enum S3TargetFormat {
    Brew {
        #[arg(long)]
        prefix: String,
        #[arg(long = "public-base-url")]
        public_base_url: String,
    },
    Deb {
        #[arg(long)]
        prefix: String,
        #[arg(long)]
        suite: String,
        #[arg(long)]
        fingerprint: String,
    },
    Rpm {
        #[arg(long)]
        prefix: String,
    },
}

pub async fn run(options: S3Options, targets: Vec<OsString>) -> Result<(), Whatever> {
    let targets = parse_s3_targets(&targets).unwrap_or_else(|error| error.exit());
    let client = client(&options).await?;
    for target in targets {
        match target {
            S3Target::Brew(target) => brew::run(&options, &client, target).await?,
            S3Target::Deb(target) => deb::run(&options, &client, target).await?,
            S3Target::Rpm(target) => rpm::run(&options, &client, target).await?,
        }
    }
    Ok(())
}

fn parse_s3_targets(tokens: &[OsString]) -> Result<Vec<S3Target>, clap::Error> {
    let sections = match grouped::parse_grouped_targets(tokens, &["deb", "rpm", "brew"]) {
        Ok(sections) => sections,
        Err(error) => return Err(target_error(ErrorKind::ValueValidation, error)),
    };
    if sections.is_empty() {
        return Err(target_error(
            ErrorKind::MissingRequiredArgument,
            "at least one s3 target is required",
        ));
    }
    sections
        .into_iter()
        .map(|section| parse_s3_target(&section.name, section.args))
        .collect()
}

fn parse_s3_target(section_name: &str, args: Vec<OsString>) -> Result<S3Target, clap::Error> {
    let mut argv = vec!["xtask publish s3".into(), section_name.to_owned().into()];
    argv.extend(args);
    S3TargetCli::try_parse_from(argv)
        .and_then(|cli| target_format_to_target(section_name, cli.target))
}

fn target_format_to_target(
    section_name: &str,
    target: S3TargetFormat,
) -> Result<S3Target, clap::Error> {
    match target {
        S3TargetFormat::Brew {
            prefix,
            public_base_url,
        } => Ok(S3Target::Brew(BrewPublishTarget {
            prefix: parse_prefix(section_name, &prefix)?,
            public_base_url: parse_public_base_url(section_name, &public_base_url)?,
        })),
        S3TargetFormat::Deb {
            prefix,
            suite,
            fingerprint,
        } => Ok(S3Target::Deb(DebPublishTarget {
            prefix: parse_prefix(section_name, &prefix)?,
            suite,
            fingerprint,
        })),
        S3TargetFormat::Rpm { prefix } => Ok(S3Target::Rpm(RpmPublishTarget {
            prefix: parse_prefix(section_name, &prefix)?,
        })),
    }
}

fn parse_prefix(section_name: &str, value: &str) -> Result<key::RemotePrefix, clap::Error> {
    match key::RemotePrefix::parse(value) {
        Ok(prefix) => Ok(prefix),
        Err(error) => Err(target_section_error(
            section_name,
            ErrorKind::ValueValidation,
            error.to_string(),
        )),
    }
}

fn parse_public_base_url(
    section_name: &str,
    value: &str,
) -> Result<key::PublicBaseUrl, clap::Error> {
    match key::PublicBaseUrl::parse(value) {
        Ok(public_base_url) => Ok(public_base_url),
        Err(error) => Err(target_section_error(
            section_name,
            ErrorKind::ValueValidation,
            error.to_string(),
        )),
    }
}

fn target_error(kind: ErrorKind, message: impl std::fmt::Display) -> clap::Error {
    S3TargetCli::command()
        .bin_name("xtask publish s3")
        .error(kind, message)
}

fn target_section_error(
    section_name: &str,
    kind: ErrorKind,
    message: impl std::fmt::Display,
) -> clap::Error {
    let mut command = S3TargetCli::command().bin_name("xtask publish s3");
    command.build();
    match command.find_subcommand_mut(section_name) {
        Some(subcommand) => subcommand.error(kind, message),
        None => command.error(kind, message),
    }
}

pub(crate) struct LoadedManifest {
    pub target_dir: PathBuf,
    pub manifest: PackageManifest,
}

pub(crate) async fn load_manifest(kind: ArtifactKind) -> Result<LoadedManifest, Whatever> {
    let target_dir = crate::target_dir()?;
    let manifest_path = target_dir
        .join("common")
        .join(kind.directory())
        .join("manifest.toml");
    let manifest = crate::package::manifest::read_manifest(&manifest_path)
        .await
        .whatever_context(format!("failed to read {}", manifest_path.display()))?;
    validate_manifest(&manifest).whatever_context("package manifest validation failed")?;
    snafu::ensure_whatever!(
        manifest.kind == kind,
        "package manifest kind does not match publish target"
    );
    Ok(LoadedManifest {
        target_dir,
        manifest,
    })
}

pub(crate) fn artifact_path(
    target_dir: &Path,
    artifact: &crate::package::PackageArtifact,
) -> PathBuf {
    target_dir.join(&artifact.path)
}

pub(crate) async fn upload_file(
    client: &Client,
    bucket: &str,
    path: &Path,
    key: &str,
    condition: Option<plan::UploadCondition>,
) -> Result<(), Whatever> {
    let body = ByteStream::from_path(path)
        .await
        .whatever_context("failed to read upload body")?;
    let mut request = client.put_object().bucket(bucket).key(key).body(body);
    if condition == Some(plan::UploadCondition::IfMissing) {
        request = request.if_none_match("*");
    }
    match request.send().await {
        Ok(_) => {}
        Err(error) if is_precondition_failed_error(&error) => {
            snafu::whatever!("remote immutable artifact {key} already exists during upload");
        }
        Err(error) => {
            snafu::whatever!("failed to upload {key}: {error}");
        }
    }
    info!(key, path = %path.display(), "uploaded package artifact");
    Ok(())
}

pub(crate) async fn plan_repository_uploads(
    client: &Client,
    bucket: &str,
    uploads: Vec<plan::PlannedUpload>,
) -> Result<Vec<plan::PlannedUpload>, Whatever> {
    let mut planned = Vec::new();
    for mut upload in uploads {
        if upload.entry {
            upload.condition = None;
            planned.push(upload);
            continue;
        }

        let actual_sha256 = crate::sha256_file(&upload.path).await?;
        let remote = remote_artifact_state(client, bucket, &upload.key).await?;
        match plan::plan_immutable_upload(&upload.key, &actual_sha256, remote)
            .whatever_context("remote package artifact collision")?
        {
            Some(condition) => {
                upload.condition = Some(condition);
                planned.push(upload);
            }
            None => {
                info!(
                    key = %upload.key,
                    path = %upload.path.display(),
                    "remote immutable package artifact already has matching sha256"
                );
            }
        }
    }
    Ok(planned)
}

pub(crate) async fn get_object_bytes(
    client: &Client,
    bucket: &str,
    key: &str,
) -> Result<Option<Vec<u8>>, Whatever> {
    let output = match client.get_object().bucket(bucket).key(key).send().await {
        Ok(output) => output,
        Err(error) if is_missing_object_error(&error) => return Ok(None),
        Err(error) => {
            snafu::whatever!("failed to fetch remote object {key}: {error}");
        }
    };
    let mut bytes = Vec::new();
    let mut body = output.body;
    while let Some(chunk) = body
        .try_next()
        .await
        .whatever_context(format!("failed to read remote object {key}"))?
    {
        bytes.extend_from_slice(&chunk);
    }
    Ok(Some(bytes))
}

pub(crate) async fn download_object(
    client: &Client,
    bucket: &str,
    key: &str,
    path: &Path,
) -> Result<(), Whatever> {
    let bytes = get_object_bytes(client, bucket, key)
        .await?
        .whatever_context(format!("remote object {key} is missing"))?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .whatever_context(format!("failed to create {}", parent.display()))?;
    }
    tokio::fs::write(path, bytes)
        .await
        .whatever_context(format!("failed to write {}", path.display()))
}

pub(crate) async fn list_object_keys(
    client: &Client,
    bucket: &str,
    prefix: &str,
) -> Result<Vec<String>, Whatever> {
    let mut paginator = client
        .list_objects_v2()
        .bucket(bucket)
        .prefix(prefix)
        .into_paginator()
        .send();
    let mut keys = Vec::new();
    while let Some(page) = paginator.next().await {
        let page = page.whatever_context(format!("failed to list s3 prefix {prefix}"))?;
        for object in page.contents() {
            if let Some(key) = object.key() {
                keys.push(key.to_string());
            }
        }
    }
    Ok(keys)
}

pub(crate) async fn remote_artifact_state(
    client: &Client,
    bucket: &str,
    key: &str,
) -> Result<plan::RemoteArtifactState, Whatever> {
    let output = match client.get_object().bucket(bucket).key(key).send().await {
        Ok(output) => output,
        Err(error) if is_missing_object_error(&error) => {
            return Ok(plan::RemoteArtifactState::Missing);
        }
        Err(error) => {
            snafu::whatever!("failed to fetch remote artifact {key}: {error}");
        }
    };
    Ok(plan::RemoteArtifactState::Present {
        sha256: sha256_stream(output.body, key).await?,
    })
}

fn is_missing_object_error(error: &SdkError<GetObjectError, impl std::fmt::Debug>) -> bool {
    if let Some(service) = error.as_service_error() {
        let metadata = service.meta();
        return classify_missing_object(metadata.code(), metadata.message(), None);
    }
    false
}

fn is_precondition_failed_error(error: &SdkError<PutObjectError, impl std::fmt::Debug>) -> bool {
    if let Some(service) = error.as_service_error() {
        let metadata = service.meta();
        return matches!(metadata.code(), Some("PreconditionFailed"));
    }
    false
}

fn classify_missing_object(code: Option<&str>, message: Option<&str>, status: Option<u16>) -> bool {
    if !matches!(status, None | Some(404)) {
        return false;
    }
    match code {
        Some("NoSuchKey") => true,
        Some("NotFound") => message
            .map(classify_not_found_message_as_object_missing)
            .unwrap_or(false),
        _ => false,
    }
}

fn classify_not_found_message_as_object_missing(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    if message.contains("bucket") {
        return false;
    }
    message.contains("key") || message.contains("object") || message.contains("not found")
}

async fn sha256_stream(mut body: ByteStream, key: &str) -> Result<String, Whatever> {
    let mut hasher = sha2::Sha256::new();
    while let Some(bytes) = body
        .try_next()
        .await
        .whatever_context(format!("failed to read remote artifact {key}"))?
    {
        hasher.update(&bytes);
    }
    Ok(crate::hex_lower(hasher.finalize().as_ref()))
}

async fn client(options: &S3Options) -> Result<Client, Whatever> {
    let credentials = Credentials::new(
        options.access_key_id.trim().to_string(),
        options.secret_access_key.trim().to_string(),
        None,
        None,
        "xtask-release",
    );
    let s3_config = aws_sdk_s3::config::Builder::new()
        .behavior_version_latest()
        .region(Region::new("auto"))
        .endpoint_url(options.endpoint_url.to_owned())
        .credentials_provider(credentials)
        .force_path_style(true)
        .build();
    Ok(Client::from_conf(s3_config))
}

#[cfg(test)]
mod tests {
    use clap::error::ErrorKind;

    use super::parse_s3_targets;

    #[test]
    fn brew_requires_public_base_url() {
        let targets = ["brew", "--prefix", "brew/pishoo"].map(std::ffi::OsString::from);
        let error = parse_s3_targets(&targets).expect_err("missing public url should fail");

        assert_eq!(error.kind(), ErrorKind::MissingRequiredArgument);
        assert!(error.to_string().contains("--public-base-url"));
    }
}
