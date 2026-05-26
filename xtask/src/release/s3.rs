use std::path::{Path, PathBuf};

use aws_credential_types::Credentials;
use aws_sdk_s3::{Client, config::Region, primitives::ByteStream};
use snafu::{OptionExt, ResultExt, Whatever};
use tracing::info;
use walkdir::WalkDir;

use super::{PublishRoot, S3Options, artifact::relative_path, paths::common_paths};

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlannedUpload {
    path: PathBuf,
    key: String,
}

pub async fn publish(options: S3Options) -> Result<(), Whatever> {
    let common = common_paths()?.root;
    let uploads = plan_uploads(&common, &options.roots, options.apt_prefix.as_deref())?;
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

    let client = client(&options).await?;
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

async fn read_secret(path: &Path) -> Result<String, Whatever> {
    let value = tokio::fs::read_to_string(path)
        .await
        .whatever_context(format!("failed to read {}", path.display()))?;
    Ok(value.trim().to_string())
}

async fn client(options: &S3Options) -> Result<Client, Whatever> {
    let access_key_id = read_secret(&options.access_key_id_file).await?;
    let secret_access_key = read_secret(&options.secret_access_key_file).await?;
    let credentials = Credentials::new(
        access_key_id,
        secret_access_key,
        None,
        None,
        "xtask-release",
    );
    let s3_config = aws_sdk_s3::config::Builder::new()
        .region(Region::new("auto"))
        .endpoint_url(options.endpoint_url.clone())
        .credentials_provider(credentials)
        .force_path_style(true)
        .build();
    Ok(Client::from_conf(s3_config))
}

fn plan_uploads(
    common: &Path,
    roots: &[PublishRoot],
    apt_prefix: Option<&str>,
) -> Result<Vec<PlannedUpload>, Whatever> {
    let mut uploads = Vec::new();
    let explicit_roots = !roots.is_empty();
    for root in selected_roots(roots) {
        let directory = root_directory(common, root);
        if !directory.exists() {
            snafu::ensure_whatever!(
                !explicit_roots,
                "requested publish root {root} is missing at {}",
                directory.display()
            );
            continue;
        }
        uploads.extend(plan_root_uploads(common, root, apt_prefix)?);
    }
    snafu::ensure_whatever!(!uploads.is_empty(), "no staged artifacts found to publish");
    uploads.sort_by(|left, right| {
        upload_order(left)
            .cmp(&upload_order(right))
            .then_with(|| left.key.cmp(&right.key))
    });
    Ok(uploads)
}

fn selected_roots(roots: &[PublishRoot]) -> Vec<PublishRoot> {
    if roots.is_empty() {
        vec![PublishRoot::Homebrew, PublishRoot::Apt]
    } else {
        roots.to_vec()
    }
}

fn plan_root_uploads(
    common: &Path,
    root: PublishRoot,
    apt_prefix: Option<&str>,
) -> Result<Vec<PlannedUpload>, Whatever> {
    let (directory, key_prefix) = match root {
        PublishRoot::Homebrew => (common.join("homebrew"), "homebrew".to_string()),
        PublishRoot::Apt => (common.join("apt"), require_apt_prefix(apt_prefix)?),
    };

    let mut uploads = Vec::new();
    for entry in WalkDir::new(&directory) {
        let entry = entry.whatever_context(format!("failed to walk {}", directory.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = relative_path(&directory, entry.path())?;
        let key = join_key(&key_prefix, &relative);
        uploads.push(PlannedUpload {
            path: entry.path().to_path_buf(),
            key,
        });
    }
    Ok(uploads)
}

fn root_directory(common: &Path, root: PublishRoot) -> PathBuf {
    match root {
        PublishRoot::Homebrew => common.join("homebrew"),
        PublishRoot::Apt => common.join("apt"),
    }
}

fn require_apt_prefix(apt_prefix: Option<&str>) -> Result<String, Whatever> {
    let prefix = apt_prefix.whatever_context("apt prefix is required when publishing apt root")?;
    let prefix = trim_slashes(prefix);
    snafu::ensure_whatever!(!prefix.is_empty(), "apt prefix must not be empty");
    Ok(prefix)
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
    use super::{PlannedUpload, plan_uploads, upload_order};
    use crate::release::PublishRoot;

    #[test]
    fn apt_pool_file_maps_under_explicit_apt_prefix() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let common = temp.path().join("common");
        let deb = common.join("apt").join("pool/main/p/pishoo/file.deb");
        std::fs::create_dir_all(deb.parent().expect("deb should have a parent"))
            .expect("deb parent should be created");
        std::fs::write(&deb, "deb").expect("deb should be written");

        let uploads = plan_uploads(&common, &[PublishRoot::Apt], Some("releases/apt"))
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
    fn explicit_homebrew_root_excludes_apt_root() {
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

        let uploads =
            plan_uploads(&common, &[PublishRoot::Homebrew], None).expect("uploads should plan");

        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].key, "homebrew/pishoo.rb");
    }

    #[test]
    fn explicit_missing_root_fails() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let common = temp.path().join("common");

        let error = plan_uploads(&common, &[PublishRoot::Homebrew], None)
            .expect_err("missing explicit root should fail");

        assert!(
            error
                .to_string()
                .starts_with("requested publish root homebrew is missing at")
        );
    }

    #[test]
    fn empty_publish_plan_fails() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let common = temp.path().join("common");

        let error = plan_uploads(&common, &[], None).expect_err("empty plan should fail");

        assert_eq!(error.to_string(), "no staged artifacts found to publish");
    }

    #[test]
    fn apt_root_requires_explicit_apt_prefix() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let common = temp.path().join("common");
        let release = common.join("apt/dists/stable/InRelease");
        std::fs::create_dir_all(release.parent().expect("release should have a parent"))
            .expect("release parent should be created");
        std::fs::write(release, "release").expect("release should be written");

        let error = plan_uploads(&common, &[PublishRoot::Apt], None)
            .expect_err("apt root without prefix should fail");

        assert_eq!(
            error.to_string(),
            "apt prefix is required when publishing apt root"
        );
    }
}
