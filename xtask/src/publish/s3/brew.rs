use std::collections::BTreeSet;

use aws_sdk_s3::Client;
use snafu::{ResultExt, Snafu, Whatever};
use tracing::info;

use super::{
    BrewPublishTarget, S3Options,
    key::{PublicBaseUrl, PublicBaseUrlError},
    plan::PlannedUpload,
};
use crate::package::manifest::{ArtifactKind, PackageArtifact, PackageManifest};

const PACKAGE_NAME: &str = "pishoo";
const FORMULA_NAME: &str = "pishoo.rb";
const DESCRIPTION: &str = "modern, secure, QUIC-powered web/proxy engine";
const HOMEPAGE: &str = "https://www.dhttp.net";
const LICENSE: &str = "Apache-2.0";
const INSTALL_CONTENT: &str = r##"  def install
    bin.install "pishoo"
    libexec.install "pishoo-worker"
    libexec.install "pishoo-ssh-session"

    (etc/"pishoo").mkpath
    chmod 0755, etc/"pishoo"
    etc.install "pishoo.conf" => "pishoo/pishoo.conf" unless File.exist? "#{etc}/pishoo/pishoo.conf"
    etc.install "mime.types"  => "pishoo/mime.types"  unless File.exist? "#{etc}/pishoo/mime.types"
  end

  def caveats
    <<~EOS
      Configuration files are installed at:
        #{etc}/pishoo/pishoo.conf
    EOS
  end

  service do
    run [opt_bin/"pishoo", "-c", etc/"pishoo/pishoo.conf"]
    keep_alive true
    log_path var/"log/pishoo.log"
    error_log_path var/"log/pishoo.error.log"
    working_dir HOMEBREW_PREFIX
  end

  test do
    system "#{bin}/pishoo", "-V"
  end"##;

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
}

pub fn render_formula(
    manifest: &PackageManifest,
    public_base_url: &str,
) -> Result<String, RenderBrewError> {
    snafu::ensure!(
        manifest.kind == ArtifactKind::Brew,
        render_brew_error::WrongKindSnafu
    );
    let base =
        PublicBaseUrl::parse(public_base_url).context(render_brew_error::PublicBaseUrlSnafu)?;
    let class_name = formula_class_name(PACKAGE_NAME);
    let mut lines = vec![
        format!("class {class_name} < Formula"),
        format!("  desc \"{}\"", escape_formula_string(DESCRIPTION)),
        format!("  version \"{}\"", escape_formula_string(&manifest.version)),
        format!("  homepage \"{}\"", escape_formula_string(HOMEPAGE)),
        format!("  license \"{}\"", escape_formula_string(LICENSE)),
        String::new(),
    ];

    for artifact in &manifest.artifacts {
        let archive_name = archive_name(artifact)?;
        let block = brew_on_block(&artifact.target)?;
        lines.extend([
            format!("  {block} do"),
            format!("    url \"{}\"", base.join(archive_name)),
            format!("    sha256 \"{}\"", artifact.sha256),
            "  end".to_string(),
            String::new(),
        ]);
    }

    let features = common_features(&manifest.artifacts)?;
    lines.push(formula_install_content(features_include_ssh_session(
        &features,
    )));
    lines.push("end".to_string());
    lines.push(String::new());
    Ok(lines.join("\n"))
}

pub async fn run(
    options: &S3Options,
    client: &Client,
    target: BrewPublishTarget,
) -> Result<(), Whatever> {
    let loaded = super::load_manifest(ArtifactKind::Brew).await?;
    let mut uploads = plan_payload_uploads(
        client,
        &options.bucket,
        &loaded.target_dir,
        &loaded.manifest,
        &target.prefix,
    )
    .await?;
    let formula = render_formula(&loaded.manifest, target.public_base_url.as_str())
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

async fn plan_payload_uploads(
    client: &Client,
    bucket: &str,
    target_dir: &std::path::Path,
    manifest: &PackageManifest,
    prefix: &super::key::RemotePrefix,
) -> Result<Vec<PlannedUpload>, Whatever> {
    let mut uploads = Vec::new();
    for artifact in &manifest.artifacts {
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
        let Some(condition) = super::plan::plan_immutable_upload(&key, &actual_sha256, remote)
            .whatever_context("remote brew artifact collision")?
        else {
            info!(
                key,
                path = %path.display(),
                "remote immutable brew artifact already has matching sha256"
            );
            continue;
        };
        uploads.push(PlannedUpload {
            path,
            key,
            entry: false,
            condition: Some(condition),
        });
    }
    Ok(uploads)
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

fn formula_install_content(include_ssh_session: bool) -> String {
    if include_ssh_session {
        return INSTALL_CONTENT.trim_end().to_string();
    }
    INSTALL_CONTENT
        .lines()
        .filter(|line| !line.contains("pishoo-ssh-session"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn formula_class_name(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

fn escape_formula_string(value: &str) -> String {
    value.replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::render_formula;
    use crate::package::manifest::{ArtifactKind, PackageArtifact, PackageManifest};

    #[test]
    fn formula_uses_public_base_url() {
        let manifest = manifest_with_features(Vec::new());

        let formula = render_formula(&manifest, "https://download.example/brew/pishoo")
            .expect("formula should render");

        assert!(formula.contains("license \"Apache-2.0\""));
        assert!(formula.contains(
            "url \"https://download.example/brew/pishoo/pishoo_0.5.2-aarch64-apple-darwin.tar.gz\""
        ));
        assert!(formula.contains("sha256 \"arm-sha\""));
        assert!(formula.contains("pishoo-worker"));
        assert!(!formula.contains("pishoo-ssh-session"));
    }

    #[test]
    fn formula_includes_ssh_session_for_sshd_feature() {
        let manifest = manifest_with_features(vec!["sshd".to_string(), "pam".to_string()]);

        let formula = render_formula(&manifest, "https://download.example/brew/pishoo")
            .expect("formula should render");

        assert!(formula.contains("pishoo-ssh-session"));
    }

    fn manifest_with_features(features: Vec<String>) -> PackageManifest {
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
}
