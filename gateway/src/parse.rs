use std::{path::Path, sync::Arc};

use dhttp::home::identity::IdentityProfile;
use snafu::ResultExt;

pub mod ast;
pub mod builtin;
pub mod diagnostic;
pub mod document;
pub mod error;
pub mod grammar;
pub mod include;
pub mod pattern;
pub mod registry;
pub mod source;
pub mod types;
pub mod value;

#[cfg(test)]
mod tests;

pub(crate) type Result<T, E = crate::error::Whatever> = std::result::Result<T, E>;

pub fn default_registry() -> registry::ConfigRegistry {
    let mut registry = registry::ConfigRegistry::new();
    builtin::register_gateway_directives(&mut registry);
    registry
}

pub fn parse_config_str_for_test(
    text: &str,
) -> Result<document::ConfigDocument, error::ConfigLoadFailure> {
    let registry = default_registry();
    load_config_text(text, None, &registry, registry::BuildOptions::default())
}

pub fn parse_server_config_str_for_test(
    text: &str,
    identity_profile: &IdentityProfile,
) -> Result<document::ConfigDocument, error::ConfigLoadFailure> {
    let registry = default_registry();
    load_config_text(
        text,
        Some(identity_profile.path()),
        &registry,
        registry::BuildOptions {
            identity_profile: Some(identity_profile),
        },
    )
}

pub fn load_config_text(
    text: &str,
    root: Option<&Path>,
    registry: &registry::ConfigRegistry,
    options: registry::BuildOptions<'_>,
) -> Result<document::ConfigDocument, error::ConfigLoadFailure> {
    load_config_text_inner(text, None, root, registry, options)
}

fn load_config_text_inner(
    text: &str,
    source_path: Option<&Path>,
    root: Option<&Path>,
    registry: &registry::ConfigRegistry,
    options: registry::BuildOptions<'_>,
) -> Result<document::ConfigDocument, error::ConfigLoadFailure> {
    let mut source_map = source::SourceMap::default();
    let source_id =
        source_map.add_source(source_path.map(Path::to_path_buf), Arc::from(text), None);

    let directives = match grammar::parse_source(text, source_id)
        .context(error::load_config_error::ParseFileSnafu { source_id })
    {
        Ok(directives) => directives,
        Err(error) => {
            return Err(error::ConfigLoadFailure {
                error,
                source_map: Arc::new(source_map),
            });
        }
    };

    let directives = match include::expand_includes(directives, &mut source_map, root)
        .context(error::load_config_error::ResolveIncludeSnafu)
    {
        Ok(directives) => directives,
        Err(error) => {
            return Err(error::ConfigLoadFailure {
                error,
                source_map: Arc::new(source_map),
            });
        }
    };

    let source_map = Arc::new(source_map);
    match registry
        .build(Arc::clone(&source_map), directives, options)
        .context(error::load_config_error::BuildDocumentSnafu)
    {
        Ok(document) => Ok(document),
        Err(error) => Err(error::ConfigLoadFailure { error, source_map }),
    }
}

#[tracing::instrument(level = "info", skip(registry), fields(path = %path.display()))]
pub async fn load_config_file(
    path: &Path,
    registry: &registry::ConfigRegistry,
    options: registry::BuildOptions<'_>,
) -> Result<document::ConfigDocument, error::ConfigLoadFailure> {
    let text = match tokio::fs::read_to_string(path).await {
        Ok(text) => text,
        Err(source) => {
            return Err(error::ConfigLoadFailure {
                error: error::LoadConfigError::ReadSource {
                    path: path.to_path_buf(),
                    source,
                },
                source_map: Arc::new(source::SourceMap::default()),
            });
        }
    };
    load_config_text_inner(&text, Some(path), path.parent(), registry, options)
}
