use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use aws_sdk_s3::Client;
use snafu::{OptionExt, ResultExt, Snafu, Whatever};
use tracing::{info, warn};

use super::{
    BrewPublishTarget, ResolvedS3Options,
    key::{PublicBaseUrl, PublicBaseUrlError},
    plan::PlannedUpload,
};
use crate::{
    package::manifest::{ArtifactKind, PackageArtifact, PackageManifest},
    release_contract::{self, ResolvedPackageMetadata},
};

const FORMULA_NAME: &str = "pishoo.rb";

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum RenderBrewError {
    #[snafu(display("brew formula requires brew package manifest"))]
    WrongKind,
    #[snafu(display("brew package artifact is missing archive name"))]
    MissingArchiveName { target: String },
    #[snafu(display("unsupported brew target {target}"))]
    UnsupportedTarget { target: String },
    #[snafu(display("brew package artifacts must use the same feature set"))]
    FeatureMismatch,
    #[snafu(display("invalid public base url"))]
    PublicBaseUrl { source: PublicBaseUrlError },
    #[snafu(display("failed to render brew template"))]
    Template {
        source: crate::template::RenderTemplateError,
    },
}

pub fn render_formula(
    manifest: &PackageManifest,
    public_base_url: &str,
    metadata: &ResolvedPackageMetadata,
    template: &str,
) -> Result<String, RenderBrewError> {
    snafu::ensure!(
        manifest.kind == ArtifactKind::Brew,
        render_brew_error::WrongKindSnafu
    );
    let base =
        PublicBaseUrl::parse(public_base_url).context(render_brew_error::PublicBaseUrlSnafu)?;
    let features = common_features(&manifest.artifacts)?;
    let variables = BTreeMap::from([
        (
            "homebrew.class".to_string(),
            formula_class_name(&metadata.name),
        ),
        (
            "homebrew.ssh_session_install".to_string(),
            if features_include_ssh_session(&features) {
                "    libexec.install \"pishoo-ssh-session\"".to_string()
            } else {
                String::new()
            },
        ),
        ("homebrew.urls".to_string(), formula_urls(manifest, &base)?),
        (
            "package.description".to_string(),
            crate::template::ruby_string(&metadata.description),
        ),
        (
            "package.homepage".to_string(),
            crate::template::ruby_string(&metadata.homepage),
        ),
        (
            "package.license".to_string(),
            crate::template::ruby_string(&metadata.license),
        ),
        (
            "package.version".to_string(),
            crate::template::ruby_string(&metadata.version),
        ),
    ]);
    crate::template::render_template(template, &variables).context(render_brew_error::TemplateSnafu)
}

pub async fn run(
    options: &ResolvedS3Options,
    client: &Client,
    target: BrewPublishTarget,
) -> Result<(), Whatever> {
    let loaded = super::load_manifest(ArtifactKind::Brew).await?;
    let (mut uploads, manifest) = plan_payload_uploads(
        client,
        &options.bucket,
        &loaded.target_dir,
        &loaded.manifest,
        &target.prefix,
    )
    .await?;
    let (metadata, template) = metadata_and_template().await?;
    let formula = render_formula(
        &manifest,
        target.public_base_url.as_str(),
        &metadata,
        &template,
    )
    .whatever_context("failed to render brew formula")?;
    let formula_path = loaded
        .target_dir
        .join("common")
        .join("brew")
        .join(FORMULA_NAME);
    uploads.push(PlannedUpload {
        path: formula_path.clone(),
        key: target.prefix.join(FORMULA_NAME),
        entry: true,
        condition: None,
    });
    uploads.sort_by(|left, right| {
        left.entry
            .cmp(&right.entry)
            .then_with(|| left.key.cmp(&right.key))
    });

    tokio::fs::write(&formula_path, formula)
        .await
        .whatever_context(format!("failed to write {}", formula_path.display()))?;

    if options.dry_run {
        for upload in &uploads {
            info!(
                key = %upload.key,
                path = %upload.path.display(),
                "would upload package artifact"
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

async fn metadata_and_template() -> Result<(ResolvedPackageMetadata, String), Whatever> {
    let contract = release_contract::load_release_contract()
        .whatever_context("failed to load release contract")?;
    let metadata = release_contract::resolve_package_metadata(&contract)
        .whatever_context("failed to resolve package metadata")?;
    let homebrew = contract
        .homebrew
        .as_ref()
        .whatever_context("release contract is missing homebrew template")?;
    let template_path = repo_root().join(&homebrew.template.path);
    let template = tokio::fs::read_to_string(&template_path)
        .await
        .whatever_context(format!("failed to read {}", template_path.display()))?;
    Ok((metadata, template))
}

fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask manifest directory should have a parent")
}

async fn plan_payload_uploads(
    client: &Client,
    bucket: &str,
    target_dir: &std::path::Path,
    manifest: &PackageManifest,
    prefix: &super::key::RemotePrefix,
) -> Result<(Vec<PlannedUpload>, PackageManifest), Whatever> {
    let mut uploads = Vec::new();
    let mut manifest = manifest.clone();
    for artifact in &mut manifest.artifacts {
        let archive_name = archive_name(artifact)
            .whatever_context("brew package artifact is missing archive name")?;
        let path = super::artifact_path(target_dir, artifact);
        let actual_sha256 = crate::sha256_file(&path).await?;
        snafu::ensure_whatever!(
            actual_sha256 == artifact.sha256,
            "sha256 mismatch for {}",
            artifact.path
        );
        let key = prefix.join(archive_name);
        let remote = super::remote_artifact_state(client, bucket, &key).await?;
        let plan = super::plan::plan_versioned_immutable_payload(&key, &actual_sha256, remote);
        artifact.sha256 = plan.metadata_sha256().to_string();
        if let Some(condition) = plan.upload_condition() {
            uploads.push(PlannedUpload {
                path,
                key,
                entry: false,
                condition: Some(condition),
            });
        } else if plan.remote_sha256_matches_local() {
            info!(
                key,
                path = %path.display(),
                "remote immutable brew artifact already has matching sha256"
            );
        } else {
            warn!(
                key,
                path = %path.display(),
                local_sha256 = %actual_sha256,
                remote_sha256 = %plan.metadata_sha256(),
                "remote immutable brew artifact already exists with different sha256; reusing remote payload for metadata"
            );
        }
    }
    Ok((uploads, manifest))
}

fn formula_urls(
    manifest: &PackageManifest,
    base: &PublicBaseUrl,
) -> Result<String, RenderBrewError> {
    let mut blocks = Vec::new();
    for artifact in &manifest.artifacts {
        let archive_name = archive_name(artifact)?;
        let block = brew_on_block(&artifact.target)?;
        blocks.push(format!(
            "  {block} do\n    url \"{}\"\n    sha256 \"{}\"\n  end",
            base.join(archive_name),
            artifact.sha256,
        ));
    }
    Ok(blocks.join("\n\n"))
}

fn archive_name(artifact: &PackageArtifact) -> Result<&str, RenderBrewError> {
    artifact
        .archive_name
        .as_deref()
        .ok_or(RenderBrewError::MissingArchiveName {
            target: artifact.target.clone(),
        })
}

fn brew_on_block(target: &str) -> Result<&'static str, RenderBrewError> {
    match target {
        "aarch64-apple-darwin" => Ok("on_arm"),
        "x86_64-apple-darwin" => Ok("on_intel"),
        _ => Err(RenderBrewError::UnsupportedTarget {
            target: target.to_string(),
        }),
    }
}

fn common_features(artifacts: &[PackageArtifact]) -> Result<BTreeSet<String>, RenderBrewError> {
    let mut feature_sets = artifacts
        .iter()
        .map(|artifact| artifact.features.iter().cloned().collect::<BTreeSet<_>>());
    let Some(features) = feature_sets.next() else {
        return Ok(BTreeSet::new());
    };
    snafu::ensure!(
        feature_sets.all(|other| other == features),
        render_brew_error::FeatureMismatchSnafu
    );
    Ok(features)
}

fn features_include_ssh_session(features: &BTreeSet<String>) -> bool {
    features.contains("sshd") || features.contains("pam")
}

fn formula_class_name(name: &str) -> String {
    let mut output = String::new();
    let mut uppercase_next = true;
    for c in name.chars() {
        if matches!(c, '-' | '_' | '.') {
            uppercase_next = true;
            continue;
        }
        if uppercase_next {
            output.extend(c.to_uppercase());
            uppercase_next = false;
        } else {
            output.push(c);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::render_formula;
    use crate::{
        package::manifest::{ArtifactKind, PackageArtifact, PackageManifest},
        release_contract::ResolvedPackageMetadata,
    };

    fn metadata() -> ResolvedPackageMetadata {
        ResolvedPackageMetadata {
            name: "pishoo".to_string(),
            version: "0.5.2".to_string(),
            description: "modern, secure, QUIC-powered web/proxy engine".to_string(),
            homepage: "https://www.dhttp.net".to_string(),
            license: "Apache-2.0".to_string(),
            repository: None,
            authors: Vec::new(),
        }
    }

    fn manifest(features: Vec<String>) -> PackageManifest {
        PackageManifest {
            schema_version: 1,
            kind: ArtifactKind::Brew,
            package: "pishoo".to_string(),
            version: "0.5.2".to_string(),
            generated_at: "2026-05-27T00:00:00Z".to_string(),
            git_commit: None,
            git_dirty: false,
            artifacts: vec![PackageArtifact {
                target: "aarch64-apple-darwin".to_string(),
                path: "aarch64-apple-darwin/release/brew/pishoo_0.5.2-aarch64-apple-darwin.tar.gz"
                    .to_string(),
                sha256: "arm-sha".to_string(),
                size: 1,
                package_name: None,
                package_version: None,
                architecture: None,
                archive_name: Some("pishoo_0.5.2-aarch64-apple-darwin.tar.gz".to_string()),
                features,
                profile: Some("release".to_string()),
            }],
        }
    }

    #[test]
    fn formula_uses_public_base_url() {
        let template = include_str!("../../../templates/pishoo.rb.in");

        let formula = render_formula(
            &manifest(Vec::new()),
            "https://download.example/brew/pishoo",
            &metadata(),
            template,
        )
        .expect("formula should render");

        assert!(formula.contains("license \"Apache-2.0\""));
        assert!(formula.contains(
            "url \"https://download.example/brew/pishoo/pishoo_0.5.2-aarch64-apple-darwin.tar.gz\""
        ));
        assert!(!formula.contains("pishoo-ssh-session"));
    }

    #[test]
    fn formula_includes_ssh_session_for_sshd_feature() {
        let template = include_str!("../../../templates/pishoo.rb.in");

        let formula = render_formula(
            &manifest(vec!["sshd".to_string()]),
            "https://download.example/brew/pishoo",
            &metadata(),
            template,
        )
        .expect("formula should render");

        assert!(formula.contains("libexec.install \"pishoo-ssh-session\""));
    }

    #[test]
    fn formula_creates_or_explains_pishoo_group() {
        let template = include_str!("../../../templates/pishoo.rb.in");

        let formula = render_formula(
            &manifest(Vec::new()),
            "https://download.example/brew/pishoo",
            &metadata(),
            template,
        )
        .expect("formula should render");

        assert!(formula.contains("def post_install\n"));
        assert!(formula.contains("/usr/bin/dscl"));
        assert!(formula.contains("/Groups/pishoo"));
        assert!(formula.contains("Process.uid.zero?"));
        assert!(formula.contains("/usr/sbin/dseditgroup"));
        assert!(formula.contains("\"-o\", \"create\", \"pishoo\""));
        assert!(formula.contains("sudo dseditgroup -o create pishoo"));
    }
}
