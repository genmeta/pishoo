use std::{error::Error, fmt};

use crate::parse::{
    error::ConfigLoadFailure,
    source::{SourceMap, SourceSpan},
};

pub struct Diagnostic<'a, E> {
    error: &'a E,
    source_map: &'a SourceMap,
}

impl<'a, E> Diagnostic<'a, E> {
    pub fn new(error: &'a E, source_map: &'a SourceMap) -> Self {
        Self { error, source_map }
    }
}

impl ConfigLoadFailure {
    pub fn diagnostic(&self) -> Diagnostic<'_, crate::parse::error::LoadConfigError> {
        Diagnostic::new(&self.error, &self.source_map)
    }
}

impl<E> fmt::Display for Diagnostic<'_, E>
where
    E: Error + 'static,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(span) = find_span(self.error) {
            render_span(f, self.source_map, span)?;
            render_include_chain(f, self.source_map, span)?;
        } else {
            write!(f, "no source span available")?;
        }
        Ok(())
    }
}

fn render_span(
    f: &mut fmt::Formatter<'_>,
    source_map: &SourceMap,
    span: SourceSpan,
) -> fmt::Result {
    let Some(source) = source_map.get(span.source_id) else {
        return write!(f, "--> unknown source");
    };
    let Some(location) = source_map.line_column(span) else {
        return write!(f, "--> unresolved source span");
    };
    let path = source
        .path
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| span.source_id.to_string());
    writeln!(f, "--> {path}:{}:{}", location.line, location.column)?;
    if let Some(line) = source_map.line_text(span) {
        writeln!(f, "   |")?;
        writeln!(f, "{:>3} | {line}", location.line)?;
        let caret_count = span.end.saturating_sub(span.start).max(1);
        let padding = " ".repeat(location.column.saturating_sub(1));
        writeln!(f, "   | {padding}{}", "^".repeat(caret_count.min(80)))?;
    }
    Ok(())
}

fn render_include_chain(
    f: &mut fmt::Formatter<'_>,
    source_map: &SourceMap,
    span: SourceSpan,
) -> fmt::Result {
    let mut current = source_map
        .get(span.source_id)
        .and_then(|source| source.included_from.clone());
    while let Some(trace) = current {
        let Some(parent) = source_map.get(trace.parent) else {
            break;
        };
        let path = parent
            .path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| trace.parent.to_string());
        if let Some(location) = source_map.line_column(trace.directive_span) {
            writeln!(
                f,
                "included from {path}:{}:{}",
                location.line, location.column
            )?;
        }
        current = parent.included_from.clone();
    }
    Ok(())
}

fn find_span(error: &(dyn Error + 'static)) -> Option<SourceSpan> {
    if let Some(error) = error.downcast_ref::<crate::parse::grammar::ParseSyntaxError>() {
        return match error {
            crate::parse::grammar::ParseSyntaxError::Syntax { span, .. } => Some(*span),
        };
    }
    if let Some(error) = error.downcast_ref::<crate::parse::error::BuildDocumentError>() {
        return match error {
            crate::parse::error::BuildDocumentError::UnknownDirective { span, .. }
            | crate::parse::error::BuildDocumentError::InvalidContext { span, .. }
            | crate::parse::error::BuildDocumentError::InvalidDirectiveShape { span, .. }
            | crate::parse::error::BuildDocumentError::DirectiveParse { span, .. } => Some(*span),
            crate::parse::error::BuildDocumentError::DuplicateDirective { duplicate, .. } => {
                Some(*duplicate)
            }
            crate::parse::error::BuildDocumentError::MissingRequiredDirective {
                context_span,
                ..
            } => Some(*context_span),
            crate::parse::error::BuildDocumentError::FinalizeContext { span, .. } => Some(*span),
        };
    }
    if let Some(error) = error.downcast_ref::<crate::parse::error::ResolveIncludeError>() {
        return match error {
            crate::parse::error::ResolveIncludeError::InvalidShape { span }
            | crate::parse::error::ResolveIncludeError::InvalidArgumentCount { span, .. }
            | crate::parse::error::ResolveIncludeError::GlobPattern { span, .. }
            | crate::parse::error::ResolveIncludeError::GlobEntry { span, .. }
            | crate::parse::error::ResolveIncludeError::ReadSource { span, .. } => Some(*span),
            crate::parse::error::ResolveIncludeError::ParseSource { source, .. } => {
                find_span(source)
            }
        };
    }
    if let Some(error) = error.downcast_ref::<crate::parse::error::ConfigQueryError>() {
        return match error {
            crate::parse::error::ConfigQueryError::MissingRequired { span, .. }
            | crate::parse::error::ConfigQueryError::TypeMismatch { span, .. }
            | crate::parse::error::ConfigQueryError::MultipleValues { span, .. }
            | crate::parse::error::ConfigQueryError::MissingChild { span, .. } => Some(*span),
        };
    }
    error.source().and_then(find_span)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::parse::{
        error::BuildDocumentError,
        source::{SourceMap, SourceSpan},
    };

    #[test]
    fn diagnostic_renders_source_line() {
        let mut sources = SourceMap::default();
        let id = sources.add_source(None, Arc::from("pishoo {\n  bad;\n}\n"), None);
        let error = BuildDocumentError::UnknownDirective {
            directive: "bad".to_owned(),
            span: SourceSpan::new(id, 11, 14),
        };
        let diagnostic = Diagnostic::new(&error, &sources).to_string();
        assert!(diagnostic.contains("bad;"));
        assert!(diagnostic.contains('^'));
        assert!(!error.to_string().contains('\n'));
    }

    #[test]
    fn diagnostic_finds_span_through_load_error_chain() {
        let mut sources = SourceMap::default();
        let id = sources.add_source(None, Arc::from("include extra.conf;\n"), None);
        let error = crate::parse::error::LoadConfigError::ResolveInclude {
            source: crate::parse::error::ResolveIncludeError::InvalidArgumentCount {
                span: SourceSpan::new(id, 0, 7),
                count: 0,
            },
        };

        let diagnostic = Diagnostic::new(&error, &sources).to_string();

        assert!(diagnostic.contains("include extra.conf;"));
        assert!(diagnostic.contains('^'));
    }

    #[test]
    fn diagnostic_renders_syntax_error_location() {
        let mut sources = SourceMap::default();
        let id = sources.add_source(None, Arc::from("pishoo {\n"), None);
        let syntax =
            crate::parse::grammar::parse_source("pishoo {\n", id).expect_err("syntax should fail");
        let error = crate::parse::error::LoadConfigError::ParseFile {
            source_id: id,
            source: syntax,
        };

        let diagnostic = Diagnostic::new(&error, &sources).to_string();

        assert!(diagnostic.contains("pishoo {"));
        assert!(diagnostic.contains('^'));
    }

    #[test]
    fn diagnostic_renders_included_source_syntax_error_location() {
        let mut sources = SourceMap::default();
        let root = sources.add_source(None, Arc::from("include child.conf;\n"), None);
        let child = sources.add_source(
            None,
            Arc::from("server {\n"),
            Some(crate::parse::source::IncludeTrace {
                parent: root,
                directive_span: SourceSpan::new(root, 0, 19),
            }),
        );
        let syntax = crate::parse::grammar::parse_source("server {\n", child)
            .expect_err("syntax should fail");
        let error = crate::parse::error::LoadConfigError::ResolveInclude {
            source: crate::parse::error::ResolveIncludeError::ParseSource {
                source_id: child,
                source: syntax,
            },
        };

        let diagnostic = Diagnostic::new(&error, &sources).to_string();

        assert!(diagnostic.contains("server {"));
        assert!(diagnostic.contains("included from"));
        assert!(diagnostic.contains('^'));
    }

    #[test]
    fn diagnostic_contains_source_snippet_but_display_is_single_line() {
        let conf = "pishoo { server { listen all 5378; server_name example.com; ssl_certificate /missing/cert.pem; ssl_certificate_key /missing/key.pem; location /api { proxy_pass ftp://backend.example.com; } } }";
        let failure =
            crate::parse::parse_config_str_for_test(conf).expect_err("config should fail");
        let report = snafu::Report::from_error(&failure.error).to_string();
        let diagnostic = failure.diagnostic().to_string();

        assert!(report.contains("unsupported proxy_pass uri scheme"));
        assert!(!failure.error.to_string().contains('\n'));
        assert!(diagnostic.contains("proxy_pass ftp://backend.example.com"));
        assert!(diagnostic.contains("^"));
    }
}
