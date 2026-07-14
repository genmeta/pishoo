use std::{path::Path, sync::Arc};

use dhttp::home::identity::IdentityProfile;
use snafu::ResultExt;

pub mod ast;
pub mod builtin;
pub mod cascade;
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
pub mod snapshot;
pub mod source;
pub mod tree;
pub mod types;
pub mod value;

pub mod keys {
    pub mod pishoo {
        use crate::parse::{
            domain::ResolvedConfigPath, registry::LocalDirectiveKey, types::StringList,
        };

        pub const PID: LocalDirectiveKey<ResolvedConfigPath> =
            crate::parse::builtin::pishoo::PID_KEY;
        pub const WORKERS: LocalDirectiveKey<StringList> =
            crate::parse::builtin::pishoo::WORKERS_KEY;
        pub const GROUPS: LocalDirectiveKey<StringList> = crate::parse::builtin::pishoo::GROUPS_KEY;
    }

    pub mod server {
        use crate::parse::{
            cascade::DirectiveKey,
            domain::ResolvedConfigPath,
            registry::{LocalDirectiveKey, RepeatedDirectiveKey},
            types::{
                AccessRulesUri, BoolConfig, DefaultType, GzipCompLevel, GzipMinLength,
                ListenConfig, MimeTypes, ResolverConfig, ServerNames, StringList,
            },
        };

        pub const LISTEN: RepeatedDirectiveKey<ListenConfig> =
            crate::parse::builtin::server::LISTEN_KEY;
        pub const SERVER_NAME: LocalDirectiveKey<ServerNames> =
            crate::parse::builtin::server::SERVER_NAME_KEY;
        pub const DNS: LocalDirectiveKey<ResolverConfig> = crate::parse::builtin::server::DNS_KEY;
        pub const GZIP: DirectiveKey<BoolConfig> = crate::parse::builtin::server::GZIP_KEY;
        pub const GZIP_VARY: DirectiveKey<BoolConfig> =
            crate::parse::builtin::server::GZIP_VARY_KEY;
        pub const GZIP_MIN_LENGTH: DirectiveKey<GzipMinLength> =
            crate::parse::builtin::server::GZIP_MIN_LENGTH_KEY;
        pub const GZIP_COMP_LEVEL: DirectiveKey<GzipCompLevel> =
            crate::parse::builtin::server::GZIP_COMP_LEVEL_KEY;
        pub const GZIP_TYPES: DirectiveKey<StringList> =
            crate::parse::builtin::server::GZIP_TYPES_KEY;
        pub const SSL_CERTIFICATE: LocalDirectiveKey<ResolvedConfigPath> =
            crate::parse::builtin::server::SSL_CERTIFICATE_KEY;
        pub const SSL_CERTIFICATE_KEY: LocalDirectiveKey<ResolvedConfigPath> =
            crate::parse::builtin::server::SSL_CERTIFICATE_KEY_KEY;
        pub const DEFAULT_TYPE: DirectiveKey<DefaultType> =
            crate::parse::builtin::server::DEFAULT_TYPE_KEY;
        pub const ACCESS_RULES: DirectiveKey<AccessRulesUri> =
            crate::parse::builtin::server::ACCESS_RULES_KEY;
        pub const RELAY: LocalDirectiveKey<BoolConfig> = crate::parse::builtin::server::RELAY_KEY;
        pub const STUN: LocalDirectiveKey<BoolConfig> = crate::parse::builtin::server::STUN_KEY;
        pub const TYPES: DirectiveKey<MimeTypes> = crate::parse::builtin::server::TYPES_KEY;
    }

    pub mod location {
        use crate::parse::{
            cascade::DirectiveKey,
            domain::ResolvedConfigPath,
            pattern::Pattern,
            registry::{ContextPayloadKey, LocalDirectiveKey, RepeatedDirectiveKey},
            types::{
                BoolConfig, DefaultType, GzipCompLevel, GzipMinLength, HeaderRules, MimeTypes,
                ProxyPass, SshLoginMethods, SshSslUsers, StringList,
            },
        };

        pub const PATTERN: ContextPayloadKey<Pattern> =
            crate::parse::builtin::location::PATTERN_KEY;
        pub const ROOT: LocalDirectiveKey<ResolvedConfigPath> =
            crate::parse::builtin::location::ROOT_KEY;
        pub const ALIAS: LocalDirectiveKey<ResolvedConfigPath> =
            crate::parse::builtin::location::ALIAS_KEY;
        pub const GZIP: DirectiveKey<BoolConfig> = crate::parse::builtin::location::GZIP_KEY;
        pub const GZIP_VARY: DirectiveKey<BoolConfig> =
            crate::parse::builtin::location::GZIP_VARY_KEY;
        pub const GZIP_MIN_LENGTH: DirectiveKey<GzipMinLength> =
            crate::parse::builtin::location::GZIP_MIN_LENGTH_KEY;
        pub const GZIP_COMP_LEVEL: DirectiveKey<GzipCompLevel> =
            crate::parse::builtin::location::GZIP_COMP_LEVEL_KEY;
        pub const GZIP_TYPES: DirectiveKey<StringList> =
            crate::parse::builtin::location::GZIP_TYPES_KEY;
        pub const INDEX: LocalDirectiveKey<StringList> = crate::parse::builtin::location::INDEX_KEY;
        pub const ADD_HEADER: RepeatedDirectiveKey<HeaderRules> =
            crate::parse::builtin::location::ADD_HEADER_KEY;
        pub const PROXY_SET_HEADER: RepeatedDirectiveKey<HeaderRules> =
            crate::parse::builtin::location::PROXY_SET_HEADER_KEY;
        pub const PROXY_PASS: LocalDirectiveKey<ProxyPass> =
            crate::parse::builtin::location::PROXY_PASS_KEY;
        pub const PROXY_SSL_CERTIFICATE: LocalDirectiveKey<ResolvedConfigPath> =
            crate::parse::builtin::location::PROXY_SSL_CERTIFICATE_KEY;
        pub const PROXY_SSL_CERTIFICATE_KEY: LocalDirectiveKey<ResolvedConfigPath> =
            crate::parse::builtin::location::PROXY_SSL_CERTIFICATE_KEY_KEY;
        pub const PROXY_SSL_TRUSTED_CERTIFICATE: LocalDirectiveKey<ResolvedConfigPath> =
            crate::parse::builtin::location::PROXY_SSL_TRUSTED_CERTIFICATE_KEY;
        pub const SSH_LOGIN: LocalDirectiveKey<SshLoginMethods> =
            crate::parse::builtin::location::SSH_LOGIN_KEY;
        pub const SSH_SSL_USER: RepeatedDirectiveKey<SshSslUsers> =
            crate::parse::builtin::location::SSH_SSL_USER_KEY;
        pub const SSH_DENY: LocalDirectiveKey<StringList> =
            crate::parse::builtin::location::SSH_DENY_KEY;
        pub const DEFAULT_TYPE: DirectiveKey<DefaultType> =
            crate::parse::builtin::location::DEFAULT_TYPE_KEY;
        pub const TYPES: DirectiveKey<MimeTypes> = crate::parse::builtin::location::TYPES_KEY;
    }
}

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
