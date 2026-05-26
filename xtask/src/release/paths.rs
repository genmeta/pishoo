#![allow(dead_code)]

use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
};

use snafu::{OptionExt, ResultExt, Whatever};
use tracing::warn;

use crate::target_dir;

#[derive(Debug, Clone)]
pub struct CommonPaths {
    pub root: PathBuf,
    pub homebrew: PathBuf,
    pub apt: PathBuf,
    pub rpm: PathBuf,
    pub manifest: PathBuf,
}

impl CommonPaths {
    pub fn new(root: PathBuf) -> Self {
        Self {
            homebrew: root.join("homebrew"),
            apt: root.join("apt"),
            rpm: root.join("rpm"),
            manifest: root.join("manifest.toml"),
            root,
        }
    }
}

pub fn common_paths() -> Result<CommonPaths, Whatever> {
    Ok(CommonPaths::new(target_dir()?.join("common")))
}

pub async fn recreate_dir(path: &Path) -> Result<(), Whatever> {
    tokio::fs::remove_dir_all(path)
        .await
        .or_else(|error| {
            if error.kind() == ErrorKind::NotFound {
                Ok(())
            } else {
                Err(error)
            }
        })
        .whatever_context(format!("failed to remove {}", path.display()))?;
    tokio::fs::create_dir_all(path)
        .await
        .whatever_context(format!("failed to create {}", path.display()))
}

pub async fn ensure_dir(path: &Path) -> Result<(), Whatever> {
    tokio::fs::create_dir_all(path)
        .await
        .whatever_context(format!("failed to create {}", path.display()))
}

pub async fn promote_staged_outputs(
    label: &str,
    tree_staging: &Path,
    tree_destination: &Path,
    manifest_staging: &Path,
    manifest_destination: &Path,
) -> Result<(), Whatever> {
    let tree_backup = tree_destination.with_file_name(format!("{label}.previous"));
    let manifest_backup = manifest_destination.with_file_name("manifest.toml.previous");
    remove_path_if_exists(&tree_backup).await?;
    remove_path_if_exists(&manifest_backup).await?;

    let mut tree_backed_up = false;
    let mut manifest_backed_up = false;
    let mut tree_promoted = false;
    let mut manifest_promoted = false;

    let result: Result<(), Whatever> = async {
        if tokio::fs::try_exists(tree_destination)
            .await
            .whatever_context(format!("failed to inspect {}", tree_destination.display()))?
        {
            tokio::fs::rename(tree_destination, &tree_backup)
                .await
                .whatever_context(format!(
                    "failed to move {} to {}",
                    tree_destination.display(),
                    tree_backup.display()
                ))?;
            tree_backed_up = true;
        }

        if tokio::fs::try_exists(manifest_destination)
            .await
            .whatever_context(format!(
                "failed to inspect {}",
                manifest_destination.display()
            ))?
        {
            tokio::fs::rename(manifest_destination, &manifest_backup)
                .await
                .whatever_context(format!(
                    "failed to move {} to {}",
                    manifest_destination.display(),
                    manifest_backup.display()
                ))?;
            manifest_backed_up = true;
        }

        tokio::fs::rename(tree_staging, tree_destination)
            .await
            .whatever_context(format!(
                "failed to move {} to {}",
                tree_staging.display(),
                tree_destination.display()
            ))?;
        tree_promoted = true;

        tokio::fs::rename(manifest_staging, manifest_destination)
            .await
            .whatever_context(format!(
                "failed to move {} to {}",
                manifest_staging.display(),
                manifest_destination.display()
            ))?;
        manifest_promoted = true;

        Ok(())
    }
    .await;

    if let Err(error) = result {
        rollback_promoted_outputs(PromotionRollback {
            tree_destination,
            tree_backup: &tree_backup,
            tree_backed_up,
            tree_promoted,
            manifest_destination,
            manifest_backup: &manifest_backup,
            manifest_backed_up,
            manifest_promoted,
        })
        .await;
        return Err(error).whatever_context(format!("failed to promote {label} staged outputs"));
    }

    if let Err(error) = remove_path_if_exists(&tree_backup).await {
        warn!(error = %snafu::Report::from_error(&error), "failed to remove previous staged tree backup after promotion");
    }
    if let Err(error) = remove_path_if_exists(&manifest_backup).await {
        warn!(error = %snafu::Report::from_error(&error), "failed to remove previous manifest backup after promotion");
    }
    Ok(())
}

struct PromotionRollback<'a> {
    tree_destination: &'a Path,
    tree_backup: &'a Path,
    tree_backed_up: bool,
    tree_promoted: bool,
    manifest_destination: &'a Path,
    manifest_backup: &'a Path,
    manifest_backed_up: bool,
    manifest_promoted: bool,
}

async fn rollback_promoted_outputs(rollback: PromotionRollback<'_>) {
    if rollback.manifest_promoted {
        remove_path_if_exists(rollback.manifest_destination)
            .await
            .unwrap_or_else(
                |error| warn!(error = %snafu::Report::from_error(&error), "failed to remove promoted manifest during rollback"),
            );
    }
    if rollback.manifest_backed_up {
        tokio::fs::rename(rollback.manifest_backup, rollback.manifest_destination)
            .await
            .unwrap_or_else(
                |error| warn!(error = %snafu::Report::from_error(&error), "failed to restore previous manifest during rollback"),
            );
    }

    if rollback.tree_promoted {
        remove_path_if_exists(rollback.tree_destination)
            .await
            .unwrap_or_else(|error| warn!(error = %snafu::Report::from_error(&error), "failed to remove promoted staged tree during rollback"));
    }
    if rollback.tree_backed_up {
        tokio::fs::rename(rollback.tree_backup, rollback.tree_destination)
            .await
            .unwrap_or_else(|error| warn!(error = %snafu::Report::from_error(&error), "failed to restore previous staged tree during rollback"));
    }
}

async fn remove_path_if_exists(path: &Path) -> Result<(), Whatever> {
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).whatever_context(format!("failed to inspect {}", path.display()));
        }
    };

    if metadata.is_dir() {
        tokio::fs::remove_dir_all(path)
            .await
            .whatever_context(format!("failed to remove {}", path.display()))
    } else {
        tokio::fs::remove_file(path)
            .await
            .whatever_context(format!("failed to remove {}", path.display()))
    }
}

pub fn normalize_s3_key(path: &Path) -> Result<String, Whatever> {
    Ok(path
        .components()
        .map(|component| {
            component
                .as_os_str()
                .to_str()
                .whatever_context("failed to convert path component to utf-8")
        })
        .collect::<Result<Vec<_>, _>>()?
        .join("/"))
}

#[cfg(test)]
mod tests {
    use std::{
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{CommonPaths, normalize_s3_key, recreate_dir};

    fn temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "gmutils-xtask-{name}-{}-{nanos}",
            std::process::id()
        ))
    }

    #[test]
    fn normalize_s3_key_uses_forward_slashes() {
        let path = Path::new("apt")
            .join("pool")
            .join("main")
            .join("g")
            .join("gmutils.deb");
        assert_eq!(
            normalize_s3_key(&path).unwrap(),
            "apt/pool/main/g/gmutils.deb"
        );
    }

    #[test]
    fn common_paths_include_rpm_directory() {
        let paths = CommonPaths::new(PathBuf::from("/tmp/target/common"));

        assert_eq!(paths.rpm, PathBuf::from("/tmp/target/common/rpm"));
    }

    #[cfg(unix)]
    #[test]
    fn normalize_s3_key_rejects_non_utf8_components() {
        use std::{ffi::OsStr, os::unix::ffi::OsStrExt};

        let path = Path::new("apt").join(OsStr::from_bytes(b"\xff"));

        let error = normalize_s3_key(&path).expect_err("non-utf8 path should fail");

        assert!(
            error
                .to_string()
                .starts_with("failed to convert path component to utf-8")
        );
    }

    #[tokio::test]
    async fn recreate_dir_accepts_missing_directory() {
        let path = temp_path("missing-dir");

        recreate_dir(&path).await.unwrap();

        assert!(path.is_dir());
        tokio::fs::remove_dir_all(path).await.unwrap();
    }

    #[tokio::test]
    async fn recreate_dir_reports_remove_errors() {
        let path = temp_path("file");
        tokio::fs::write(&path, b"not a directory").await.unwrap();

        let error = recreate_dir(&path)
            .await
            .expect_err("file path should fail removal as a directory");

        assert!(error.to_string().starts_with("failed to remove "));
        tokio::fs::remove_file(path).await.unwrap();
    }
}
