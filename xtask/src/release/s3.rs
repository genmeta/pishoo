use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use aws_credential_types::Credentials;
use aws_sdk_s3::{
    Client, config::Region, error::SdkError, operation::get_object::GetObjectError,
    primitives::ByteStream,
};
use clap::{Args, CommandFactory, Parser, Subcommand, error::ErrorKind};
use sha2::Digest;
use snafu::{ResultExt, Whatever};
use tracing::info;
use walkdir::WalkDir;

use super::{
    S3Options, S3VerifyOptions,
    artifact::{
        ArtifactEntry, ArtifactRoot, ReleaseManifest, read_manifest, relative_path, sha256_file,
    },
    grouped,
    paths::common_paths,
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlannedUpload {
    path: PathBuf,
    key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3TargetPlan {
    pub root: ArtifactRoot,
    pub prefix: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteArtifactState {
    Missing,
    Present { sha256: String },
}

#[derive(Debug, Parser)]
struct TargetCli {
    #[command(subcommand)]
    target: TargetFormat,
}

#[derive(Debug, Subcommand)]
enum TargetFormat {
    /// Homebrew artifacts under the homebrew prefix
    Homebrew,
    /// APT artifacts under an explicit prefix
    Apt {
        #[command(flatten)]
        options: PrefixOptions,
    },
    /// RPM artifacts under an explicit prefix
    Rpm {
        #[command(flatten)]
        options: PrefixOptions,
    },
}

#[derive(Debug, Clone, Args)]
struct PrefixOptions {
    /// Remote prefix for this target
    #[arg(long)]
    prefix: String,
}

trait S3ConnectionOptions {
    fn endpoint_url(&self) -> &str;
    fn bucket(&self) -> &str;
    fn access_key_id(&self) -> &str;
    fn secret_access_key(&self) -> &str;
}

impl S3ConnectionOptions for S3Options {
    fn endpoint_url(&self) -> &str {
        &self.endpoint_url
    }

    fn bucket(&self) -> &str {
        &self.bucket
    }

    fn access_key_id(&self) -> &str {
        &self.access_key_id
    }

    fn secret_access_key(&self) -> &str {
        &self.secret_access_key
    }
}

impl S3ConnectionOptions for S3VerifyOptions {
    fn endpoint_url(&self) -> &str {
        &self.endpoint_url
    }

    fn bucket(&self) -> &str {
        &self.bucket
    }

    fn access_key_id(&self) -> &str {
        &self.access_key_id
    }

    fn secret_access_key(&self) -> &str {
        &self.secret_access_key
    }
}

pub async fn publish(options: S3Options, targets: Vec<OsString>) -> Result<(), Whatever> {
    let plans = parse_publish_target_plans(&targets).unwrap_or_else(|error| {
        error.exit();
    });
    let common = common_paths()?.root;
    let uploads = plan_uploads(&common, &plans)?;
    if options.dry_run {
        for upload in uploads {
            info!(
                "would upload {} -> s3://{}/{}",
                upload.path.display(),
                options.bucket,
                upload.key
            );
        }
        return Ok(());
    }

    let manifest = read_manifest(&common.join("manifest.toml")).await?;
    let client = client(&options).await?;
    verify_remote_artifacts(&client, options.bucket(), &common, &manifest, &plans).await?;

    for upload in uploads {
        client
            .put_object()
            .bucket(&options.bucket)
            .key(&upload.key)
            .body(
                ByteStream::from_path(&upload.path)
                    .await
                    .whatever_context("failed to read upload body")?,
            )
            .send()
            .await
            .whatever_context(format!("failed to upload {}", upload.key))?;
        info!(key = %upload.key, "uploaded staged artifact");
    }
    Ok(())
}

pub async fn verify_remote(
    options: S3VerifyOptions,
    targets: Vec<OsString>,
) -> Result<(), Whatever> {
    let plans = parse_target_plans(&targets).unwrap_or_else(|error| {
        error.exit();
    });
    let common = common_paths()?.root;
    let manifest = read_manifest(&common.join("manifest.toml")).await?;
    verify_remote_manifest_targets(&manifest, &plans)?;
    let client = client(&options).await?;

    verify_remote_artifacts(&client, options.bucket(), &common, &manifest, &plans).await
}

async fn verify_remote_artifacts(
    client: &Client,
    bucket: &str,
    common: &Path,
    manifest: &ReleaseManifest,
    plans: &[S3TargetPlan],
) -> Result<(), Whatever> {
    verify_remote_manifest_targets(manifest, plans)?;
    for plan in plans {
        for artifact in remote_plan_artifacts(manifest, plan) {
            let path = common.join(artifact.root.directory()).join(&artifact.path);
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

            let key = join_key(&plan.prefix, &artifact.path);
            let remote = remote_artifact_state(client, bucket, &key).await?;
            verify_immutable_collision(&key, &actual, remote)?;
        }
    }
    Ok(())
}

fn verify_remote_manifest_targets(
    manifest: &ReleaseManifest,
    plans: &[S3TargetPlan],
) -> Result<(), Whatever> {
    for plan in plans {
        snafu::ensure_whatever!(
            !remote_plan_artifacts(manifest, plan).is_empty(),
            "remote release target {} has no immutable manifest artifacts",
            plan.root.directory()
        );
    }
    Ok(())
}

fn remote_plan_artifacts<'a>(
    manifest: &'a ReleaseManifest,
    plan: &S3TargetPlan,
) -> Vec<&'a ArtifactEntry> {
    manifest
        .artifacts
        .iter()
        .filter(|artifact| artifact.root == plan.root && artifact.immutable)
        .collect()
}

async fn remote_artifact_state(
    client: &Client,
    bucket: &str,
    key: &str,
) -> Result<RemoteArtifactState, Whatever> {
    let output = match client.get_object().bucket(bucket).key(key).send().await {
        Ok(output) => output,
        Err(error) if is_missing_object_error(&error) => return Ok(RemoteArtifactState::Missing),
        Err(error) => {
            snafu::whatever!("failed to fetch remote artifact {key}: {error}");
        }
    };
    Ok(RemoteArtifactState::Present {
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
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn verify_immutable_collision(
    artifact_path: &str,
    local_sha256: &str,
    remote: RemoteArtifactState,
) -> Result<(), Whatever> {
    match remote {
        RemoteArtifactState::Missing => Ok(()),
        RemoteArtifactState::Present { sha256 } if sha256 == local_sha256 => Ok(()),
        RemoteArtifactState::Present { sha256 } => {
            snafu::whatever!(
                "remote immutable artifact {artifact_path} already exists with different sha256 {sha256}"
            )
        }
    }
}

async fn client(options: &impl S3ConnectionOptions) -> Result<Client, Whatever> {
    let credentials = Credentials::new(
        options.access_key_id().trim().to_string(),
        options.secret_access_key().trim().to_string(),
        None,
        None,
        "xtask-release",
    );
    let s3_config = aws_sdk_s3::config::Builder::new()
        .region(Region::new("auto"))
        .endpoint_url(options.endpoint_url().to_owned())
        .credentials_provider(credentials)
        .force_path_style(true)
        .build();
    Ok(Client::from_conf(s3_config))
}

fn plan_uploads(common: &Path, plans: &[S3TargetPlan]) -> Result<Vec<PlannedUpload>, Whatever> {
    let mut uploads = Vec::new();
    for plan in plans {
        let directory = common.join(plan.root.directory());
        snafu::ensure_whatever!(
            directory.exists(),
            "requested publish target {} is missing at {}",
            plan.root.directory(),
            directory.display()
        );
        uploads.extend(plan_root_uploads(&directory, &plan.prefix)?);
    }
    snafu::ensure_whatever!(!uploads.is_empty(), "no staged artifacts found to publish");
    uploads.sort_by(|left, right| {
        upload_order(left)
            .cmp(&upload_order(right))
            .then_with(|| left.key.cmp(&right.key))
    });
    Ok(uploads)
}

fn plan_root_uploads(directory: &Path, key_prefix: &str) -> Result<Vec<PlannedUpload>, Whatever> {
    let mut uploads = Vec::new();
    for entry in WalkDir::new(directory) {
        let entry = entry.whatever_context(format!("failed to walk {}", directory.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = relative_path(directory, entry.path())?;
        let key = join_key(key_prefix, &relative);
        uploads.push(PlannedUpload {
            path: entry.path().to_path_buf(),
            key,
        });
    }
    Ok(uploads)
}

fn trim_slashes(value: &str) -> String {
    value.trim_matches('/').to_string()
}

fn join_key(prefix: &str, relative: &str) -> String {
    if prefix.is_empty() {
        relative.to_string()
    } else {
        format!("{prefix}/{relative}")
    }
}

pub fn parse_target_plans(tokens: &[OsString]) -> Result<Vec<S3TargetPlan>, clap::Error> {
    parse_target_plans_with_command("xtask verify remote s3", tokens)
}

fn parse_publish_target_plans(tokens: &[OsString]) -> Result<Vec<S3TargetPlan>, clap::Error> {
    parse_target_plans_with_command("xtask publish s3", tokens)
}

fn parse_target_plans_with_command(
    command_name: &'static str,
    tokens: &[OsString],
) -> Result<Vec<S3TargetPlan>, clap::Error> {
    let sections = grouped::parse_grouped_targets(tokens, &["homebrew", "apt", "rpm"])
        .map_err(|error| target_error(command_name, ErrorKind::ValueValidation, error))?;
    if sections.is_empty() {
        return Err(target_error(
            command_name,
            ErrorKind::MissingRequiredArgument,
            "at least one s3 target is required",
        ));
    }

    sections
        .into_iter()
        .map(|section| parse_target_plan(command_name, &section.name, section.args))
        .collect()
}

fn parse_target_plan(
    command_name: &'static str,
    section_name: &str,
    args: Vec<OsString>,
) -> Result<S3TargetPlan, clap::Error> {
    let mut argv = vec![OsString::from(command_name), section_name.to_owned().into()];
    argv.extend(args);
    TargetCli::try_parse_from(argv)
        .and_then(|cli| target_format_to_plan(command_name, section_name, cli.target))
}

fn target_format_to_plan(
    command_name: &'static str,
    section_name: &str,
    target: TargetFormat,
) -> Result<S3TargetPlan, clap::Error> {
    match target {
        TargetFormat::Homebrew => Ok(S3TargetPlan {
            root: ArtifactRoot::Homebrew,
            prefix: "homebrew".to_string(),
        }),
        TargetFormat::Apt { options } => Ok(S3TargetPlan {
            root: ArtifactRoot::Apt,
            prefix: validate_target_prefix(command_name, section_name, options.prefix)?,
        }),
        TargetFormat::Rpm { options } => Ok(S3TargetPlan {
            root: ArtifactRoot::Rpm,
            prefix: validate_target_prefix(command_name, section_name, options.prefix)?,
        }),
    }
}

fn validate_target_prefix(
    command_name: &'static str,
    section_name: &str,
    prefix: String,
) -> Result<String, clap::Error> {
    let prefix = trim_slashes(&prefix);
    if prefix.is_empty() {
        return Err(target_section_error(
            command_name,
            section_name,
            ErrorKind::ValueValidation,
            "prefix must not be empty",
        ));
    }
    Ok(prefix)
}

fn target_error(
    command_name: &'static str,
    kind: ErrorKind,
    message: impl std::fmt::Display,
) -> clap::Error {
    TargetCli::command()
        .bin_name(command_name)
        .error(kind, message)
}

fn target_section_error(
    command_name: &'static str,
    section_name: &str,
    kind: ErrorKind,
    message: impl std::fmt::Display,
) -> clap::Error {
    let mut command = TargetCli::command().bin_name(command_name);
    command.build();
    match command.find_subcommand_mut(section_name) {
        Some(subcommand) => subcommand.error(kind, message),
        None => command.error(kind, message),
    }
}

fn upload_order(upload: &PlannedUpload) -> u8 {
    let key = upload.key.as_str();
    if key.contains("/pool/") || key.starts_with("pool/") {
        return 0;
    }
    if key.ends_with(".tar.gz")
        || key.ends_with(".zip")
        || key.ends_with(".deb")
        || key.ends_with(".rpm")
    {
        return 1;
    }
    if key.ends_with("InRelease") {
        return 4;
    }
    if key.ends_with(".json") || key.ends_with(".rb") {
        return 3;
    }
    2
}

#[cfg(test)]
mod tests {
    use clap::error::ErrorKind;

    use super::{
        PlannedUpload, RemoteArtifactState, S3TargetPlan, classify_missing_object,
        parse_publish_target_plans, parse_target_plans, plan_uploads, upload_order,
        verify_immutable_collision, verify_remote_manifest_targets,
    };
    use crate::release::artifact::{ArtifactEntry, ArtifactRoot, ReleaseManifest};

    #[test]
    fn apt_pool_file_maps_under_explicit_apt_prefix() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let common = temp.path().join("common");
        let deb = common.join("apt").join("pool/main/p/pishoo/file.deb");
        std::fs::create_dir_all(deb.parent().expect("deb should have a parent"))
            .expect("deb parent should be created");
        std::fs::write(&deb, "deb").expect("deb should be written");

        let uploads = plan_uploads(
            &common,
            &[S3TargetPlan {
                root: ArtifactRoot::Apt,
                prefix: "releases/apt".to_string(),
            }],
        )
        .expect("uploads should plan");

        assert!(
            uploads
                .iter()
                .any(|upload| upload.key == "releases/apt/pool/main/p/pishoo/file.deb")
        );
    }

    #[test]
    fn inrelease_sorts_after_release_gpg() {
        let release_gpg = PlannedUpload {
            path: "Release.gpg".into(),
            key: "apt/stable/dists/stable/Release.gpg".to_string(),
        };
        let in_release = PlannedUpload {
            path: "InRelease".into(),
            key: "apt/stable/dists/stable/InRelease".to_string(),
        };

        assert!(upload_order(&release_gpg) < upload_order(&in_release));
    }

    #[test]
    fn explicit_homebrew_target_excludes_apt_root() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let common = temp.path().join("common");
        for path in [
            common.join("homebrew/pishoo.rb"),
            common.join("apt/dists/stable/InRelease"),
        ] {
            std::fs::create_dir_all(path.parent().expect("path should have a parent"))
                .expect("parent should be created");
            std::fs::write(path, "artifact").expect("artifact should be written");
        }

        let uploads = plan_uploads(
            &common,
            &[S3TargetPlan {
                root: ArtifactRoot::Homebrew,
                prefix: "homebrew".to_string(),
            }],
        )
        .expect("uploads should plan");

        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].key, "homebrew/pishoo.rb");
    }

    #[test]
    fn explicit_missing_target_fails() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let common = temp.path().join("common");

        let error = plan_uploads(
            &common,
            &[S3TargetPlan {
                root: ArtifactRoot::Homebrew,
                prefix: "homebrew".to_string(),
            }],
        )
        .expect_err("missing explicit target should fail");

        assert!(
            error
                .to_string()
                .starts_with("requested publish target homebrew is missing at")
        );
    }

    #[test]
    fn empty_publish_plan_fails() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let common = temp.path().join("common");

        let error = plan_uploads(&common, &[]).expect_err("empty plan should fail");

        assert_eq!(error.to_string(), "no staged artifacts found to publish");
    }

    #[test]
    fn grouped_publish_plans_each_target_under_its_prefix() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let common = temp.path().join("common");
        for path in [
            common.join("homebrew/pishoo.rb"),
            common.join("apt/pool/main/p/pishoo/file.deb"),
            common.join("rpm/pishoo/0.5.1/file.rpm"),
        ] {
            std::fs::create_dir_all(path.parent().expect("path should have a parent"))
                .expect("parent should be created");
            std::fs::write(path, "artifact").expect("artifact should be written");
        }
        let plans = [
            S3TargetPlan {
                root: ArtifactRoot::Homebrew,
                prefix: "homebrew".to_string(),
            },
            S3TargetPlan {
                root: ArtifactRoot::Apt,
                prefix: "apt/genmeta".to_string(),
            },
            S3TargetPlan {
                root: ArtifactRoot::Rpm,
                prefix: "rpm/genmeta".to_string(),
            },
        ];

        let uploads = plan_uploads(&common, &plans).expect("uploads should plan");
        let keys = uploads
            .iter()
            .map(|upload| upload.key.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            keys,
            vec![
                "apt/genmeta/pool/main/p/pishoo/file.deb",
                "rpm/genmeta/pishoo/0.5.1/file.rpm",
                "homebrew/pishoo.rb",
            ]
        );
    }

    #[test]
    fn s3_targets_parse_prefixes_per_target() {
        let targets = [
            "homebrew",
            "apt",
            "--prefix",
            "/download/apt/",
            "rpm",
            "--prefix",
            "download/rpm",
        ]
        .map(std::ffi::OsString::from);

        let plans = parse_target_plans(&targets).expect("s3 targets should parse");

        assert_eq!(plans.len(), 3);
        assert_eq!(plans[0].root, ArtifactRoot::Homebrew);
        assert_eq!(plans[0].prefix, "homebrew");
        assert_eq!(plans[1].root, ArtifactRoot::Apt);
        assert_eq!(plans[1].prefix, "download/apt");
        assert_eq!(plans[2].root, ArtifactRoot::Rpm);
        assert_eq!(plans[2].prefix, "download/rpm");
    }

    #[test]
    fn s3_target_help_is_neutral_for_verify_and_publish() {
        let verify = parse_target_plans(&[
            std::ffi::OsString::from("homebrew"),
            std::ffi::OsString::from("--help"),
        ])
        .expect_err("verify target-local help should be returned");
        let publish = parse_publish_target_plans(&[
            std::ffi::OsString::from("apt"),
            std::ffi::OsString::from("--help"),
        ])
        .expect_err("publish target-local help should be returned");

        assert_eq!(verify.kind(), ErrorKind::DisplayHelp);
        assert!(
            verify
                .to_string()
                .contains("Homebrew artifacts under the homebrew prefix")
        );
        assert!(!verify.to_string().contains("Verify Homebrew"));
        assert_eq!(publish.kind(), ErrorKind::DisplayHelp);
        assert!(
            publish
                .to_string()
                .contains("APT artifacts under an explicit prefix")
        );
        assert!(!publish.to_string().contains("Verify APT"));
        assert!(publish.to_string().contains("Usage: xtask publish s3 apt"));
    }

    #[test]
    fn s3_targets_apt_requires_prefix() {
        let error = parse_target_plans(&[std::ffi::OsString::from("apt")])
            .expect_err("apt target without prefix should fail");

        assert!(error.to_string().contains("--prefix"));
        assert!(
            error
                .to_string()
                .contains("Usage: xtask verify remote s3 apt")
        );
    }

    #[test]
    fn s3_targets_rpm_requires_prefix() {
        let error = parse_target_plans(&[std::ffi::OsString::from("rpm")])
            .expect_err("rpm target without prefix should fail");

        assert!(error.to_string().contains("--prefix"));
        assert!(
            error
                .to_string()
                .contains("Usage: xtask verify remote s3 rpm")
        );
    }

    #[test]
    fn publish_s3_targets_apt_requires_prefix_with_publish_usage() {
        let error = parse_publish_target_plans(&[std::ffi::OsString::from("apt")])
            .expect_err("apt publish target without prefix should fail");

        assert!(error.to_string().contains("--prefix"));
        assert!(error.to_string().contains("Usage: xtask publish s3 apt"));
    }

    #[test]
    fn immutable_collision_missing_passes() {
        verify_immutable_collision("homebrew/file.tar.gz", "abc", RemoteArtifactState::Missing)
            .expect("missing remote artifact should pass");
    }

    #[test]
    fn immutable_collision_same_hash_passes() {
        verify_immutable_collision(
            "homebrew/file.tar.gz",
            "abc",
            RemoteArtifactState::Present {
                sha256: "abc".to_string(),
            },
        )
        .expect("matching remote artifact should pass");
    }

    #[test]
    fn immutable_collision_different_hash_fails() {
        let error = verify_immutable_collision(
            "homebrew/file.tar.gz",
            "abc",
            RemoteArtifactState::Present {
                sha256: "def".to_string(),
            },
        )
        .expect_err("different remote artifact should fail");

        assert_eq!(
            error.to_string(),
            "remote immutable artifact homebrew/file.tar.gz already exists with different sha256 def"
        );
    }

    #[test]
    fn missing_object_classifier_accepts_only_object_missing_errors() {
        assert!(classify_missing_object(Some("NoSuchKey"), None, Some(404)));
        assert!(classify_missing_object(
            Some("NotFound"),
            Some("object not found"),
            Some(404)
        ));
        assert!(classify_missing_object(
            Some("NotFound"),
            Some("key does not exist"),
            Some(404)
        ));
    }

    #[test]
    fn missing_object_classifier_rejects_bucket_and_generic_404_errors() {
        assert!(!classify_missing_object(
            Some("NoSuchBucket"),
            Some("bucket not found"),
            Some(404)
        ));
        assert!(!classify_missing_object(
            Some("NotFound"),
            Some("bucket not found"),
            Some(404)
        ));
        assert!(!classify_missing_object(Some("NotFound"), None, Some(404)));
        assert!(!classify_missing_object(None, None, Some(404)));
        assert!(!classify_missing_object(
            Some("AccessDenied"),
            None,
            Some(404)
        ));
        assert!(!classify_missing_object(Some("NoSuchKey"), None, Some(500)));
    }

    #[test]
    fn remote_verify_target_requires_matching_immutable_manifest_artifact() {
        let manifest = ReleaseManifest {
            schema_version: 1,
            package: "pishoo".to_string(),
            version: "0.5.1".to_string(),
            artifacts: vec![ArtifactEntry {
                root: ArtifactRoot::Rpm,
                path: "pishoo.rpm".to_string(),
                sha256: "abc".to_string(),
                immutable: false,
            }],
        };
        let plans = vec![S3TargetPlan {
            root: ArtifactRoot::Rpm,
            prefix: "rpm".to_string(),
        }];

        let error = verify_remote_manifest_targets(&manifest, &plans)
            .expect_err("explicit remote target without immutable artifacts should fail");

        assert_eq!(
            error.to_string(),
            "remote release target rpm has no immutable manifest artifacts"
        );
    }
}
