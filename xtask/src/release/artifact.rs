#![allow(dead_code)]

use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::Digest;
use snafu::{OptionExt, ResultExt, Whatever};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct ReleaseManifest {
    pub schema_version: u32,
    pub package: String,
    pub version: String,
    pub artifacts: Vec<ArtifactEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct ArtifactEntry {
    pub root: ArtifactRoot,
    pub path: String,
    pub sha256: String,
    pub immutable: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactRoot {
    Homebrew,
    Scoop,
    Ppa,
}

pub async fn sha256_file(path: &Path) -> Result<String, Whatever> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::open(&path)
            .whatever_context(format!("failed to open {}", path.display()))?;
        let mut hasher = sha2::Sha256::new();
        std::io::copy(&mut file, &mut hasher)
            .whatever_context(format!("failed to read {}", path.display()))?;
        Ok(format!("{:x}", hasher.finalize()))
    })
    .await
    .whatever_context("sha256 task panicked")?
}

pub async fn copy_artifact(src: &Path, dst: &Path) -> Result<(), Whatever> {
    if let Some(parent) = dst.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .whatever_context(format!("failed to create {}", parent.display()))?;
    }
    tokio::fs::copy(src, dst).await.whatever_context(format!(
        "failed to copy {} to {}",
        src.display(),
        dst.display()
    ))?;
    Ok(())
}

pub async fn write_manifest(path: &Path, manifest: &ReleaseManifest) -> Result<(), Whatever> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .whatever_context(format!("failed to create {}", parent.display()))?;
    }
    let content = toml::to_string_pretty(manifest)
        .whatever_context("failed to serialize release manifest")?;
    tokio::fs::write(path, content)
        .await
        .whatever_context(format!("failed to write {}", path.display()))
}

pub async fn read_manifest(path: &Path) -> Result<ReleaseManifest, Whatever> {
    let content = tokio::fs::read_to_string(path)
        .await
        .whatever_context(format!("failed to read {}", path.display()))?;
    toml::from_str(&content).whatever_context(format!("failed to parse {}", path.display()))
}

pub fn relative_path(root: &Path, file: &Path) -> Result<String, Whatever> {
    let relative = file.strip_prefix(root).whatever_context(format!(
        "failed to make {} relative to {}",
        file.display(),
        root.display()
    ))?;
    Ok(relative
        .components()
        .map(|component| {
            component
                .as_os_str()
                .to_str()
                .whatever_context("failed to convert relative path component to utf-8")
        })
        .collect::<Result<Vec<_>, _>>()?
        .join("/"))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{ArtifactEntry, ArtifactRoot, ReleaseManifest, relative_path};

    #[test]
    fn manifest_serializes_roots_in_kebab_case() {
        let manifest = ReleaseManifest {
            schema_version: 1,
            package: "gmutils".to_string(),
            version: "0.5.1".to_string(),
            artifacts: vec![ArtifactEntry {
                root: ArtifactRoot::Homebrew,
                path: "gmutils-0.5.1-x86_64-apple-darwin.tar.gz".to_string(),
                sha256: "abc".to_string(),
                immutable: true,
            }],
        };
        let text = toml::to_string(&manifest).unwrap();
        assert!(text.contains("homebrew"));
        assert!(text.contains("schema-version"));
        assert!(!text.contains("schema_version"));
    }

    #[test]
    fn relative_path_error_starts_with_semantic_context() {
        let error = relative_path(Path::new("/root"), Path::new("/other/file"))
            .expect_err("file should not be relative to root");
        let text = error.to_string();
        assert!(text.starts_with("failed to make /other/file relative to /root"));
    }

    #[cfg(unix)]
    #[test]
    fn relative_path_rejects_non_utf8_components() {
        use std::{ffi::OsStr, os::unix::ffi::OsStrExt};

        let root = Path::new("/root");
        let file = root.join(OsStr::from_bytes(b"\xff"));

        let error = relative_path(root, &file).expect_err("non-utf8 path should fail");

        assert!(
            error
                .to_string()
                .starts_with("failed to convert relative path component to utf-8")
        );
    }
}
