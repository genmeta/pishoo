use std::{fmt, path::PathBuf, sync::Arc};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SourceId(pub(crate) u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SourceSpan {
    pub source_id: SourceId,
    pub start: usize,
    pub end: usize,
}

impl SourceSpan {
    pub fn new(source_id: SourceId, start: usize, end: usize) -> Self {
        Self {
            source_id,
            start,
            end,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

#[derive(Debug, Clone)]
pub struct IncludeTrace {
    pub parent: SourceId,
    pub directive_span: SourceSpan,
}

#[derive(Debug, Clone)]
pub struct SourceFile {
    pub id: SourceId,
    pub path: Option<PathBuf>,
    pub text: Arc<str>,
    pub line_starts: Vec<usize>,
    pub included_from: Option<IncludeTrace>,
}

#[derive(Debug, Default)]
pub struct SourceMap {
    sources: Vec<SourceFile>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineColumn {
    pub line: usize,
    pub column: usize,
}

impl SourceMap {
    pub fn add_source(
        &mut self,
        path: Option<PathBuf>,
        text: Arc<str>,
        included_from: Option<IncludeTrace>,
    ) -> SourceId {
        let id = SourceId(self.sources.len() as u32);
        let line_starts = line_starts(&text);
        self.sources.push(SourceFile {
            id,
            path,
            text,
            line_starts,
            included_from,
        });
        id
    }

    pub fn get(&self, id: SourceId) -> Option<&SourceFile> {
        self.sources.get(id.0 as usize)
    }

    pub fn line_column(&self, span: SourceSpan) -> Option<LineColumn> {
        let source = self.get(span.source_id)?;
        let line_index = match source.line_starts.binary_search(&span.start) {
            Ok(index) => index,
            Err(index) => index.saturating_sub(1),
        };
        let line_start = source.line_starts[line_index];
        Some(LineColumn {
            line: line_index + 1,
            column: span.start.saturating_sub(line_start) + 1,
        })
    }

    pub fn line_text(&self, span: SourceSpan) -> Option<&str> {
        let source = self.get(span.source_id)?;
        let lc = self.line_column(span)?;
        let start = *source.line_starts.get(lc.line - 1)?;
        let end = source
            .line_starts
            .get(lc.line)
            .copied()
            .unwrap_or_else(|| source.text.len());
        Some(source.text[start..end].trim_end_matches(['\r', '\n']))
    }
}

impl fmt::Display for SourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "source:{}", self.0)
    }
}

fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (index, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            starts.push(index + 1);
        }
    }
    starts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_map_resolves_line_and_column() {
        let mut sources = SourceMap::default();
        let id = sources.add_source(None, Arc::from("one\ntwo\nthree"), None);

        assert_eq!(
            sources.line_column(SourceSpan::new(id, 4, 7)),
            Some(LineColumn { line: 2, column: 1 })
        );
        assert_eq!(
            sources.line_column(SourceSpan::new(id, 5, 8)),
            Some(LineColumn { line: 2, column: 2 })
        );
        assert_eq!(sources.line_text(SourceSpan::new(id, 4, 7)), Some("two"));
    }
}
