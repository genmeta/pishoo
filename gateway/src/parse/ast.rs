use crate::parse::source::SourceSpan;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spanned<T> {
    pub value: T,
    pub span: SourceSpan,
}

impl<T> Spanned<T> {
    pub fn new(value: T, span: SourceSpan) -> Self {
        Self { value, span }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AstDirective {
    pub name: Spanned<String>,
    pub args: Vec<Spanned<String>>,
    pub body: AstBody,
    pub span: SourceSpan,
}

impl AstDirective {
    pub fn is_leaf(&self) -> bool {
        matches!(self.body, AstBody::Leaf { .. })
    }

    pub fn children(&self) -> Option<&[AstDirective]> {
        match &self.body {
            AstBody::Leaf { .. } => None,
            AstBody::Block { children, .. } => Some(children),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AstBody {
    Leaf {
        semicolon: SourceSpan,
    },
    Block {
        open: SourceSpan,
        children: Vec<AstDirective>,
        close: SourceSpan,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::source::{SourceId, SourceSpan};

    #[test]
    fn ast_distinguishes_leaf_and_block() {
        let source = SourceId(0);
        let span = SourceSpan::new(source, 0, 4);
        let directive = AstDirective {
            name: Spanned::new("pid".to_owned(), span),
            args: Vec::new(),
            body: AstBody::Leaf { semicolon: span },
            span,
        };
        assert!(directive.is_leaf());
        assert!(directive.children().is_none());
    }
}
