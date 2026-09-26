//! Direct-child library directory discovery and lib ID checks.

use std::{
    fs,
    path::{Path, PathBuf},
};

use super::{Result, invalid};

pub fn valid_lib_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 63
        && id.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

pub(super) fn direct_directories(root: &Path) -> Result<Vec<(String, PathBuf)>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    if !root.is_dir() || root.symlink_metadata()?.file_type().is_symlink() {
        return Err(invalid(format!("not a safe directory: {}", root.display())).into());
    }
    let mut result = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        // file_type does not follow symlinks.
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        result.push((name, entry.path()));
    }
    result.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(result)
}
