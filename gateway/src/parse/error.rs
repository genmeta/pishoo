use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use snafu::Snafu;

use crate::parse::{
    domain::{ConfigDocumentId, ConfigDocumentIdError},
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
    #[snafu(display("failed to build typed configuration"))]
    BuildTyped {
        source: crate::parse::build::BuildTypedConfigError,
    },
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
    pub(crate) fn read(path: &Path, source: std::io::Error) -> Self {
        Self {
            error: LoadConfigError::ReadSource {
                path: path.to_path_buf(),
                source,
            },
            source_map: Arc::new(SourceMap::default()),
            document_id: None,
        }
    }
}
