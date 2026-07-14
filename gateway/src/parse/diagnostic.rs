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
    if let Some(error) = error.downcast_ref::<crate::parse::build::BuildTypedConfigError>() {
        use crate::parse::build::BuildTypedConfigError::*;
        return Some(match error {
            UnknownDirective { span, .. }
            | Shape { span, .. }
            | Directive { span, .. }
            | Missing { span, .. }
            | PishooCardinality { span }
            | WorkerServer { span }
            | ServerTlsPair { span }
            | StandaloneTls { span }
            | AmbiguousProfile { span }
            | IdentityName { span }
            | IdentityTls { span }
            | ProxyTlsPair { span }
            | RegexProxyUri { span }
            | DirectDefaultAccessLog { span }
            | AccessLogArgument { span }
            | ForwardListen { span } => *span,
            Duplicate { duplicate, .. } => *duplicate,
        });
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
    error.source().and_then(find_span)
}
