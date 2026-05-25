use std::{io, sync::Arc};

use snafu::{ResultExt, Snafu};
use tokio::fs::File;

use crate::parse::{document::ConfigNode, types::StringList};

#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum IndexError {
    #[snafu(display("missing index files while serving directory"))]
    MissingIndexFiles,

    #[snafu(display("file was not found at `{path}`"))]
    FileNotFound { path: String },

    #[snafu(display("failed to read file metadata at `{path}`"))]
    ReadMetadata { source: io::Error, path: String },

    #[snafu(display("failed to open file at `{path}`"))]
    OpenFile { source: io::Error, path: String },
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
    node: &Arc<ConfigNode>,
    file_path: impl Into<String>,
) -> Result<(File, u64, String), IndexError> {
    let mut file_path = file_path.into();
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
        if !file_path.ends_with('/') {
            file_path.push('/');
        }
        let base_dir_path = file_path.clone();

        let index_files = node
            .get::<StringList>("index")
            .ok()
            .flatten()
            .map(|index| index.0.clone())
            .ok_or(IndexError::MissingIndexFiles)?;

        for index_filename in index_files {
            let mut potential_path = base_dir_path.clone();
            potential_path.push_str(&index_filename);

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
