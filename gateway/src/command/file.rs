use std::{
    io,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use snafu::{ResultExt, Snafu};
use tokio::fs::File;

use crate::parse::config::LocationConfig;

#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum SafePathError {
    #[snafu(display("path contains parent directory segment"))]
    ParentDir,

    #[snafu(display("path contains root directory segment"))]
    RootDir,

    #[snafu(display("path contains path prefix"))]
    Prefix,

    #[snafu(display("path contains backslash separator"))]
    Backslash,
}

#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum IndexError {
    #[snafu(display("missing index files while serving directory"))]
    MissingIndexFiles,

    #[snafu(display("file was not found at `{}`", path.display()))]
    FileNotFound { path: PathBuf },

    #[snafu(display("failed to read file metadata at `{}`", path.display()))]
    ReadMetadata { source: io::Error, path: PathBuf },

    #[snafu(display("failed to open file at `{}`", path.display()))]
    OpenFile { source: io::Error, path: PathBuf },

    #[snafu(display("unsafe index file path `{index}`"))]
    UnsafeIndexPath {
        index: String,
        source: SafePathError,
    },
}

pub(crate) fn safe_relative_path(path: &str) -> Result<PathBuf, SafePathError> {
    let mut result = PathBuf::new();

    for component in Path::new(path.trim_start_matches('/')).components() {
        match component {
            Component::Prefix(_) => return PrefixSnafu.fail(),
            Component::RootDir => return RootDirSnafu.fail(),
            Component::CurDir => {}
            Component::ParentDir => return ParentDirSnafu.fail(),
            Component::Normal(part) => {
                if part.as_encoded_bytes().contains(&b'\\') {
                    return BackslashSnafu.fail();
                }
                result.push(part);
            }
        }
    }

    Ok(result)
}

/// Attempts to open a file directly or serve an index file if the path points to a directory.
///
/// If `file_path` refers to a regular file, it opens that file.
/// If `file_path` refers to a directory, it searches for index files within that directory
/// based on the configuration found in the `node` (specifically looking for a "index" key
/// under an "index" sub-node). It attempts to open the first valid index file found.
///
/// # Arguments
///
/// * `node` - The config node used to retrieve the list of potential index filenames.
/// * `file_path` - The path to the file or directory to serve.
///
/// # Returns
///
/// Returns a `Result` containing a tuple `(File, u64)` on success, where `File` is the
/// opened file handle and `u64` is the file size.
/// Returns an `io::Error` if the path doesn't exist, if it's a directory without a
/// suitable index file, or if file/metadata operations fail.
pub(crate) async fn index(
    node: &Arc<LocationConfig>,
    file_path: impl Into<PathBuf>,
) -> Result<(File, u64, PathBuf), IndexError> {
    let file_path = file_path.into();
    let metadata = match tokio::fs::metadata(&file_path).await {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(IndexError::FileNotFound { path: file_path });
        }
        Err(source) => {
            return Err(IndexError::ReadMetadata {
                source,
                path: file_path,
            });
        }
    };

    if metadata.is_file() {
        return File::open(&file_path)
            .await
            .map(|file| (file, metadata.len(), file_path.clone()))
            .context(OpenFileSnafu { path: file_path });
    }

    // 2. 检查是否是目录
    if metadata.is_dir() {
        let base_dir_path = file_path.clone();

        let index_files = node
            .index()
            .map(|index| index.0.clone())
            .ok_or(IndexError::MissingIndexFiles)?;

        for index_filename in index_files {
            let relative_index =
                safe_relative_path(&index_filename).context(UnsafeIndexPathSnafu {
                    index: index_filename,
                })?;
            let potential_path = base_dir_path.join(relative_index);

            if let Ok(metadata) = tokio::fs::metadata(&*potential_path).await
                && metadata.is_file()
            {
                return File::open(&*potential_path)
                    .await
                    .map(|file| (file, metadata.len(), potential_path.clone()))
                    .context(OpenFileSnafu {
                        path: potential_path,
                    });
            }
        }
    }

    Err(IndexError::FileNotFound { path: file_path })
}

#[cfg(test)]
mod tests {
    use std::{
        sync::Arc,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;
    use crate::parse::tests::parse_location;

    fn node_with_indexes(indexes: Vec<&str>) -> Arc<LocationConfig> {
        parse_location(&format!("index {};", indexes.join(" "))).unwrap()
    }

    fn temp_dir(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("{name}-{}-{stamp}", std::process::id()))
    }

    #[test]
    fn safe_relative_path_rejects_parent_segments() {
        let error = safe_relative_path("/../secret.txt").expect_err("parent segment should fail");

        assert!(matches!(error, SafePathError::ParentDir));
    }

    #[tokio::test]
    async fn index_rejects_parent_dir_index_filename() {
        let root = temp_dir("gateway-index-traversal");
        let public = root.join("public");
        std::fs::create_dir_all(&public).expect("create public dir");
        std::fs::write(root.join("secret.txt"), b"secret").expect("write secret");

        let error = index(&node_with_indexes(vec!["../secret.txt"]), &public)
            .await
            .expect_err("unsafe index path should fail");

        assert!(matches!(error, IndexError::UnsafeIndexPath { .. }));

        std::fs::remove_dir_all(root).expect("remove temp tree");
    }
}
