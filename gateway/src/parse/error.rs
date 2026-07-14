use std::{path::PathBuf, sync::Arc};

use snafu::Snafu;

use crate::parse::{
    domain::{
        ConfigDocumentId, ConfigDocumentIdError, ConfigDocumentRoleKind, ConfigSourceSpan,
        DirectiveName,
    },
    grammar::ParseSyntaxError,
    registry::{CascadePolicy, ContextKey},
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
    DocumentRole { source: ConfigDocumentRoleError },
}

#[derive(Debug, Snafu)]
#[snafu(module, visibility(pub(crate)))]
pub enum ConfigDocumentRoleError {
    #[snafu(display("directive `{directive}` is not allowed in a {role} configuration document"))]
    DirectiveNotAllowed {
        directive: DirectiveName,
        role: ConfigDocumentRoleKind,
        span: ConfigSourceSpan,
    },

    #[snafu(display(
        "{role} configuration document must contain exactly one top-level pishoo block, found {found}"
    ))]
    ExpectedSinglePishoo {
        role: ConfigDocumentRoleKind,
        found: usize,
        span: ConfigSourceSpan,
    },

    #[snafu(display(
        "identity server configuration document must contain at least one server block"
    ))]
    MissingIdentityServer {
        role: ConfigDocumentRoleKind,
        span: ConfigSourceSpan,
    },

    #[snafu(display(
        "directive `{directive}` has an invalid registry contract for a {role} configuration document; expected a top-level `{expected_child_context}` context block"
    ))]
    InvalidDirectiveRegistration {
        directive: DirectiveName,
        role: ConfigDocumentRoleKind,
        expected_child_context: ContextKey,
        span: ConfigSourceSpan,
    },

    #[snafu(display("directive `{directive}` was not built for a {role} configuration document"))]
    MissingBuiltDirective {
        directive: DirectiveName,
        role: ConfigDocumentRoleKind,
        span: ConfigSourceSpan,
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
            role,
            span,
        }
    }

    pub(crate) fn expected_single_pishoo(
        role: ConfigDocumentRoleKind,
        found: usize,
        span: ConfigSourceSpan,
    ) -> Self {
        Self::ExpectedSinglePishoo { role, found, span }
    }

    pub(crate) fn missing_identity_server(
        role: ConfigDocumentRoleKind,
        span: ConfigSourceSpan,
    ) -> Self {
        Self::MissingIdentityServer { role, span }
    }

    pub(crate) fn invalid_directive_registration(
        directive: DirectiveName,
        role: ConfigDocumentRoleKind,
        expected_child_context: ContextKey,
        span: ConfigSourceSpan,
    ) -> Self {
        Self::InvalidDirectiveRegistration {
            directive,
            role,
            expected_child_context,
            span,
        }
    }

    pub(crate) fn missing_built_directive(
        directive: DirectiveName,
        role: ConfigDocumentRoleKind,
        span: ConfigSourceSpan,
    ) -> Self {
        Self::MissingBuiltDirective {
            directive,
            role,
            span,
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

    #[snafu(display("configuration node parent was assigned more than once"))]
    ParentAlreadyAssigned { span: SourceSpan },
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

    #[snafu(display(
        "directive `{directive}` has inconsistent cascade policies: context `{inherited_context}` uses {inherited:?}, context `{local_context}` uses {local:?}"
    ))]
    CascadePolicyMismatch {
        directive: String,
        inherited_context: ContextKey,
        inherited: CascadePolicy,
        local_context: ContextKey,
        local: CascadePolicy,
    },

    #[snafu(display(
        "directive `{directive}` has an incompatible contract in context `{context}`: {mismatch}"
    ))]
    ContractMismatch {
        directive: DirectiveName,
        context: ContextKey,
        mismatch: crate::parse::registry::DirectiveContractMismatch,
    },

    #[snafu(display("directive `{directive}` has no registered cascade policy"))]
    MissingCascadePolicy { directive: String },

    #[snafu(display("directive `{directive}` does not support typed cascading with {policy:?}"))]
    UnsupportedCascadePolicy {
        directive: String,
        policy: CascadePolicy,
    },
}

#[derive(Debug)]
pub struct ConfigLoadFailure {
    pub error: LoadConfigError,
    pub source_map: Arc<SourceMap>,
    pub(crate) document_id: Option<ConfigDocumentId>,
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
    pub fn document_id(&self) -> Option<ConfigDocumentId> {
        self.document_id
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
            document_id: None,
        };

        assert_eq!(failure.to_string(), "failed to load configuration");
        assert!(std::error::Error::source(&failure).is_some());
    }

    #[test]
    fn cascade_policy_mismatch_display_names_both_contexts() {
        let error = ConfigQueryError::CascadePolicyMismatch {
            directive: "gzip".to_owned(),
            inherited_context: crate::parse::registry::context::PISHOO,
            inherited: CascadePolicy::NearestWins,
            local_context: crate::parse::registry::context::SERVER,
            local: CascadePolicy::ReplaceWhole,
        };
        let rendered = error.to_string();

        assert!(rendered.contains(crate::parse::registry::context::PISHOO.0));
        assert!(rendered.contains(crate::parse::registry::context::SERVER.0));
        assert!(rendered.contains("NearestWins"));
        assert!(rendered.contains("ReplaceWhole"));
    }
}
