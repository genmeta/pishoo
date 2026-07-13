use std::{path::Path, sync::Arc};

use dhttp::home::identity::IdentityProfile;
use snafu::ResultExt;

pub mod ast;
pub mod builtin;
pub mod diagnostic;
pub mod document;
pub mod domain;
pub mod error;
pub mod fragment;
pub mod grammar;
pub mod include;
pub mod normalize;
pub mod pattern;
pub mod registry;
pub mod source;
pub mod types;
pub mod value;

pub struct ConfigDocumentParser<'registry> {
    registry: &'registry registry::ConfigRegistry,
    document_ids: domain::ConfigDocumentIdAllocator,
}

impl<'registry> ConfigDocumentParser<'registry> {
    pub fn new(registry: &'registry registry::ConfigRegistry) -> Self {
        Self {
            registry,
            document_ids: domain::ConfigDocumentIdAllocator::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_next_document_index(
        registry: &'registry registry::ConfigRegistry,
        next_index: u64,
    ) -> Self {
        Self {
            registry,
            document_ids: domain::ConfigDocumentIdAllocator::with_next_index(next_index),
        }
    }

    pub fn parse_text(
        &mut self,
        text: &str,
        source_path: &Path,
        role: domain::ConfigDocumentRole<'_>,
    ) -> Result<fragment::ParsedConfigDocument, error::ConfigLoadFailure> {
        let document_id = match self.document_ids.allocate() {
            Ok(document_id) => document_id,
            Err(source) => {
                return Err(error::ConfigLoadFailure {
                    error: error::LoadConfigError::DocumentId { source },
                    source_map: Arc::new(source::SourceMap::default()),
                    document_id: None,
                });
            }
        };
        let role_kind = role.kind();
        let options = role.build_options();
        let source_path = source_path.to_path_buf();
        let mut source_map = source::SourceMap::default();
        let source_id = source_map.add_source(
            Some(source_path.clone()),
            Arc::from(text),
            source_path.parent().map(Path::to_path_buf),
            None,
        );

        let directives = match grammar::parse_source(text, source_id)
            .context(error::load_config_error::ParseFileSnafu { source_id })
        {
            Ok(directives) => directives,
            Err(error) => {
                return Err(error::ConfigLoadFailure {
                    error,
                    source_map: Arc::new(source_map),
                    document_id: Some(document_id),
                });
            }
        };

        let directives =
            match include::expand_includes(directives, &mut source_map, source_path.parent())
                .context(error::load_config_error::ResolveIncludeSnafu)
            {
                Ok(directives) => directives,
                Err(error) => {
                    return Err(error::ConfigLoadFailure {
                        error,
                        source_map: Arc::new(source_map),
                        document_id: Some(document_id),
                    });
                }
            };

        let source_map = Arc::new(source_map);
        let document_sources = Arc::new(source::ConfigDocumentSourceMap::new(
            document_id,
            Arc::clone(&source_map),
        ));
        match self
            .registry
            .build_for_role(document_sources, directives, options, role_kind)
        {
            Ok(document) => Ok(document),
            Err(registry::RoleDocumentBuildError::Role(source)) => Err(error::ConfigLoadFailure {
                error: error::LoadConfigError::DocumentRole { source },
                source_map,
                document_id: Some(document_id),
            }),
            Err(registry::RoleDocumentBuildError::Build(source)) => Err(error::ConfigLoadFailure {
                error: error::LoadConfigError::BuildDocument { source },
                source_map,
                document_id: Some(document_id),
            }),
        }
    }
}

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
    let home_path = identity_profile
        .path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| identity_profile.path().to_path_buf());
    let home = dhttp::home::DhttpHome::new(home_path);
    load_config_text(
        text,
        Some(identity_profile.path()),
        &registry,
        registry::BuildOptions {
            dhttp_home: Some(&home),
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
    let source_id = source_map.add_source(
        source_path.map(Path::to_path_buf),
        Arc::from(text),
        root.map(Path::to_path_buf),
        None,
    );

    let directives = match grammar::parse_source(text, source_id)
        .context(error::load_config_error::ParseFileSnafu { source_id })
    {
        Ok(directives) => directives,
        Err(error) => {
            return Err(error::ConfigLoadFailure {
                error,
                source_map: Arc::new(source_map),
                document_id: None,
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
                document_id: None,
            });
        }
    };

    let source_map = Arc::new(source_map);
    match registry
        .build(Arc::clone(&source_map), directives, options)
        .context(error::load_config_error::BuildDocumentSnafu)
    {
        Ok(document) => Ok(document),
        Err(error) => Err(error::ConfigLoadFailure {
            error,
            source_map,
            document_id: None,
        }),
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
                document_id: None,
            });
        }
    };
    load_config_text_inner(&text, Some(path), path.parent(), registry, options)
}
