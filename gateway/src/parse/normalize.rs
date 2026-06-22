use std::path::{Path, PathBuf};

use snafu::{OptionExt, Snafu};

use crate::parse::{
    source::{SourceMap, SourceSpan},
    types::PathConfig,
    value::TypedValue,
};

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum NormalizeDirectiveValueError {
    #[snafu(display("relative path requires a configuration file base directory"))]
    MissingBaseDir { span: SourceSpan },
}

pub fn normalize_path(
    path: &Path,
    span: SourceSpan,
    source_map: &SourceMap,
) -> Result<PathBuf, NormalizeDirectiveValueError> {
    if path.is_absolute() {
        return Ok(path.components().collect());
    }

    let base_dir = source_map
        .base_dir_for_span(span)
        .context(normalize_directive_value_error::MissingBaseDirSnafu { span })?;
    Ok(base_dir.join(path).components().collect())
}

pub fn normalize_slot_value(
    value: TypedValue,
    source_map: &SourceMap,
) -> Result<TypedValue, NormalizeDirectiveValueError> {
    let span = value.span();
    if let Some(path) = value.downcast::<PathConfig>() {
        let normalized = normalize_path(&path.0, span, source_map)?;
        return Ok(TypedValue::new(PathConfig(normalized), span));
    }

    Ok(value)
}
