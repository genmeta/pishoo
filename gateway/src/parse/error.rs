use std::{path::PathBuf, sync::Arc};

use snafu::Snafu;

use crate::parse::{
    grammar::ParseSyntaxError,
    source::{SourceId, SourceMap, SourceSpan},
};

#[derive(Debug, Snafu)]
#[snafu(module, visibility(pub(crate)))]
pub enum LoadConfigError {
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
