use std::error::Error;

use super::{
    ast::AstDirective,
    source::{SourceMap, SourceSpan},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigContext {
    Pishoo,
    Server,
    Location,
}

pub struct DirectiveInput<'a> {
    pub directive: &'a AstDirective,
    pub context: ConfigContext,
    pub source_map: &'a SourceMap,
}

pub trait DirectiveValue: Sized {
    type Error: Error + Send + Sync + 'static;

    fn span(input: &DirectiveInput<'_>) -> SourceSpan {
        input.directive.span
    }
}
