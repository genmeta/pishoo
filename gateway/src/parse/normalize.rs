use std::path::{Path, PathBuf};

use snafu::{OptionExt, ResultExt, Snafu};

use crate::parse::{
    domain::{ResolvedConfigPath, ResolvedConfigPathError},
    source::{SourceMap, SourceSpan},
};

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum NormalizeDirectiveValueError {
    #[snafu(display("relative path requires a configuration file base directory"))]
    MissingBaseDir { span: SourceSpan },
    #[snafu(display("configuration path is not a resolved absolute path"))]
    InvalidResolvedPath {
        span: SourceSpan,
        source: ResolvedConfigPathError,
    },
}

pub(crate) fn resolve_config_path(
    path: &Path,
    span: SourceSpan,
    source_map: &SourceMap,
) -> Result<ResolvedConfigPath, NormalizeDirectiveValueError> {
    let path: PathBuf = if path.is_absolute() {
        path.components().collect()
    } else {
        let base_dir = source_map
            .base_dir_for_span(span)
            .context(normalize_directive_value_error::MissingBaseDirSnafu { span })?;
        base_dir.join(path).components().collect()
    };
    ResolvedConfigPath::try_from(path)
        .context(normalize_directive_value_error::InvalidResolvedPathSnafu { span })
}

pub(crate) fn normalize_path(
    path: &Path,
    span: SourceSpan,
    source_map: &SourceMap,
) -> Result<PathBuf, NormalizeDirectiveValueError> {
    resolve_config_path(path, span, source_map).map(Into::into)
}
