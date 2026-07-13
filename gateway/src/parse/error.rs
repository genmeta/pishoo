use std::{path::PathBuf, sync::Arc};

use snafu::Snafu;

use crate::parse::{
    domain::{
        ConfigDocumentId, ConfigDocumentIdError, ConfigDocumentRoleKind, ConfigSourceSpan,
        DirectiveName,
    },
    grammar::ParseSyntaxError,
    source::{SourceId, SourceMap, SourceSpan},
};

#[derive(Debug, Snafu)]
#[snafu(module, visibility(pub(crate)))]
pub enum LoadConfigError {
    #[snafu(display("failed to allocate configuration document identity"))]
    DocumentId { source: ConfigDocumentIdError },

    #[snafu(display("failed to read configuration source"))]
    ReadSource {
        path: PathBuf,
        source: std::io::Error,
    },

    #[snafu(display("failed to parse configuration file"))]
    ParseFile {
        source_id: SourceId,
        source: ParseSyntaxError,
    },

    #[snafu(display("failed to resolve configuration includes"))]
    ResolveInclude { source: ResolveIncludeError },

    #[snafu(display("failed to build configuration document"))]
    BuildDocument { source: BuildDocumentError },

    #[snafu(display("configuration document is invalid for its role"))]
    DocumentRole {
        source: Box<ConfigDocumentRoleError>,
    },
}

#[derive(Debug, Snafu)]
#[snafu(module, visibility(pub(crate)))]
pub enum ConfigDocumentRoleError {
    #[snafu(display("directive `{directive}` is not allowed in a {role} configuration document"))]
    DirectiveNotAllowed {
        directive: DirectiveName,
        role: &'static str,
        span: ConfigSourceSpan,
        source: Box<BuildDocumentError>,
    },

    #[snafu(display(
        "{role} configuration document must contain exactly one top-level pishoo block, found {found}"
    ))]
    ExpectedSinglePishoo {
        role: &'static str,
        found: usize,
        span: ConfigSourceSpan,
        source: Box<BuildDocumentError>,
    },

    #[snafu(display(
        "identity server configuration document must contain at least one server block"
    ))]
    MissingIdentityServer {
        span: ConfigSourceSpan,
        source: Box<BuildDocumentError>,
    },
}

impl ConfigDocumentRoleError {
    pub(crate) fn directive_not_allowed(
        directive: DirectiveName,
        role: ConfigDocumentRoleKind,
        span: ConfigSourceSpan,
    ) -> Self {
        Self::DirectiveNotAllowed {
            directive,
            role: role.as_str(),
            span,
            source: Box::new(BuildDocumentError::InvalidContext {
                directive: directive.as_str().to_owned(),
                context: role.as_str(),
                span: span.source_span(),
            }),
        }
    }

    pub(crate) fn expected_single_pishoo(
        role: ConfigDocumentRoleKind,
        found: usize,
        span: ConfigSourceSpan,
        first: Option<SourceSpan>,
    ) -> Self {
        let source = match first {
            Some(first) => BuildDocumentError::DuplicateDirective {
                directive: "pishoo".to_owned(),
                first,
                duplicate: span.source_span(),
            },
            None => BuildDocumentError::MissingRequiredDirective {
                directive: "pishoo",
                context_span: span.source_span(),
            },
        };
        Self::ExpectedSinglePishoo {
            role: role.as_str(),
            found,
            span,
            source: Box::new(source),
        }
    }

    pub(crate) fn missing_identity_server(span: ConfigSourceSpan) -> Self {
        Self::MissingIdentityServer {
            span,
            source: Box::new(BuildDocumentError::MissingRequiredDirective {
                directive: "server",
                context_span: span.source_span(),
            }),
        }
    }
}

#[derive(Debug, Snafu)]
#[snafu(module, visibility(pub(crate)))]
pub enum ResolveIncludeError {
    #[snafu(display("invalid include directive shape"))]
    InvalidShape { span: SourceSpan },

    #[snafu(display("invalid include directive argument count"))]
    InvalidArgumentCount { span: SourceSpan, count: usize },

    #[snafu(display("invalid include glob pattern"))]
    GlobPattern {
        span: SourceSpan,
        pattern: String,
        source: glob::PatternError,
    },

    #[snafu(display("failed to resolve include glob entry"))]
    GlobEntry {
        span: SourceSpan,
        pattern: String,
        source: glob::GlobError,
    },

    #[snafu(display("failed to read included source"))]
    ReadSource {
        span: SourceSpan,
        path: PathBuf,
        source: std::io::Error,
    },

    #[snafu(display("failed to parse included source"))]
    ParseSource {
        source_id: SourceId,
        source: ParseSyntaxError,
    },
}

#[derive(Debug, Snafu)]
#[snafu(module, visibility(pub(crate)))]
pub enum BuildDocumentError {
    #[snafu(display("unknown directive `{directive}`"))]
    UnknownDirective { directive: String, span: SourceSpan },

    #[snafu(display("directive `{directive}` is not valid in this context"))]
    InvalidContext {
        directive: String,
        context: &'static str,
        span: SourceSpan,
    },

    #[snafu(display("invalid directive shape for `{directive}`"))]
    InvalidDirectiveShape { directive: String, span: SourceSpan },

    #[snafu(display("failed to parse directive `{directive}`"))]
    DirectiveParse {
        directive: String,
        span: SourceSpan,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[snafu(display("duplicate directive `{directive}`"))]
    DuplicateDirective {
        directive: String,
        first: SourceSpan,
        duplicate: SourceSpan,
    },

    #[snafu(display("failed to normalize directive `{directive}`"))]
    NormalizeDirectiveValue {
        directive: String,
        span: SourceSpan,
        source: crate::parse::normalize::NormalizeDirectiveValueError,
    },

    #[snafu(display("missing required directive `{directive}`"))]
    MissingRequiredDirective {
        directive: &'static str,
        context_span: SourceSpan,
    },

    #[snafu(display("failed to finalize configuration context"))]
    FinalizeContext {
        context: &'static str,
        span: SourceSpan,
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

#[derive(Debug, Snafu)]
#[snafu(module, visibility(pub(crate)))]
pub enum ConfigQueryError {
    #[snafu(display("missing required directive `{directive}`"))]
    MissingRequired { directive: String, span: SourceSpan },

    #[snafu(display("directive `{directive}` has unexpected value type"))]
    TypeMismatch {
        directive: String,
        expected: &'static str,
        actual: &'static str,
        span: SourceSpan,
    },

    #[snafu(display("directive `{directive}` has multiple values"))]
    MultipleValues { directive: String, span: SourceSpan },

    #[snafu(display("missing child directive `{directive}`"))]
    MissingChild { directive: String, span: SourceSpan },
}

#[derive(Debug)]
pub struct ConfigLoadFailure {
    pub error: LoadConfigError,
    pub source_map: Arc<SourceMap>,
}

impl std::fmt::Display for ConfigLoadFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "failed to load configuration")
    }
}

impl std::error::Error for ConfigLoadFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

impl ConfigLoadFailure {
    pub fn document_id(&self) -> ConfigDocumentId {
        self.source_map.document_id()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::source::{SourceId, SourceMap, SourceSpan};

    #[test]
    fn config_load_failure_display_describes_only_wrapper() {
        let failure = ConfigLoadFailure {
            error: LoadConfigError::ResolveInclude {
                source: ResolveIncludeError::InvalidArgumentCount {
                    span: SourceSpan::new(SourceId(0), 0, 7),
                    count: 0,
                },
            },
            source_map: Arc::new(SourceMap::default()),
        };

        assert_eq!(failure.to_string(), "failed to load configuration");
        assert!(std::error::Error::source(&failure).is_some());
    }
}
