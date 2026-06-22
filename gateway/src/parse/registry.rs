use std::{collections::HashMap, error::Error, sync::Arc};

use snafu::{OptionExt, ResultExt};

use crate::parse::{
    ast::{AstBody, AstDirective},
    document::{ConfigDocument, ConfigNode},
    error::{BuildDocumentError, build_document_error},
    normalize,
    source::{SourceMap, SourceSpan},
    value::{ConfigValue, TypedValue},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContextKey(pub &'static str);

pub mod context {
    use super::ContextKey;

    pub const ROOT: ContextKey = ContextKey("gateway.root");
    pub const PISHOO: ContextKey = ContextKey("gateway.pishoo");
    pub const SERVER: ContextKey = ContextKey("gateway.server");
    pub const LOCATION: ContextKey = ContextKey("gateway.location");
    pub const PROXY: ContextKey = ContextKey("gateway.proxy");
    pub const STUN_SERVER: ContextKey = ContextKey("gateway.stun_server");
}

#[derive(Default)]
pub struct ConfigRegistry {
    contexts: HashMap<ContextKey, ContextSpec>,
    directives: HashMap<(ContextKey, &'static str), DirectiveSpec>,
}

pub struct ContextSpec {
    pub key: ContextKey,
    pub finalize: Option<ContextFinalizeFn>,
}

pub struct DirectiveSpec {
    pub name: &'static str,
    pub allowed_in: Vec<ContextKey>,
    pub shape: DirectiveShape,
    parser: DirectiveParserFn,
    pub merge: MergePolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectiveShape {
    Leaf,
    ContextBlock {
        child_context: ContextKey,
        payload: PayloadMode,
    },
    RawBlock,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadMode {
    None,
    Parser,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergePolicy {
    RejectDuplicate,
    LastWins,
    Append,
    InheritIfMissing,
    MergeWithParent,
}

type DirectiveParserFn =
    fn(&DirectiveInput<'_>) -> Result<ParsedDirective, Box<dyn std::error::Error + Send + Sync>>;
pub type ContextFinalizeFn =
    fn(&mut ConfigNode, &BuildOptions<'_>) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

pub trait DirectiveValue: ConfigValue + Sized {
    type Error: Error + Send + Sync + 'static;

    fn span(input: &DirectiveInput<'_>) -> SourceSpan {
        input.directive.span
    }
}

pub struct DirectiveInput<'a> {
    pub directive: &'a AstDirective,
    pub context: ContextKey,
    pub source_map: &'a SourceMap,
}

pub enum ParsedDirective {
    Slot(TypedValue),
    Payload(TypedValue),
    Empty,
}

impl DirectiveSpec {
    pub fn leaf_value<T>(
        name: &'static str,
        allowed_in: Vec<ContextKey>,
        merge: MergePolicy,
    ) -> Self
    where
        T: DirectiveValue,
        for<'input, 'directive> T:
            TryFrom<&'input DirectiveInput<'directive>, Error = <T as DirectiveValue>::Error>,
    {
        Self {
            name,
            allowed_in,
            shape: DirectiveShape::Leaf,
            parser: slot_value_parser::<T>,
            merge,
        }
    }

    pub fn raw_value<T>(name: &'static str, allowed_in: Vec<ContextKey>, merge: MergePolicy) -> Self
    where
        T: DirectiveValue,
        for<'input, 'directive> T:
            TryFrom<&'input DirectiveInput<'directive>, Error = <T as DirectiveValue>::Error>,
    {
        Self {
            name,
            allowed_in,
            shape: DirectiveShape::RawBlock,
            parser: slot_value_parser::<T>,
            merge,
        }
    }

    pub fn context_empty(
        name: &'static str,
        allowed_in: Vec<ContextKey>,
        child_context: ContextKey,
        merge: MergePolicy,
    ) -> Self {
        Self {
            name,
            allowed_in,
            shape: DirectiveShape::ContextBlock {
                child_context,
                payload: PayloadMode::None,
            },
            parser: empty_parser,
            merge,
        }
    }

    pub fn context_payload<T>(
        name: &'static str,
        allowed_in: Vec<ContextKey>,
        child_context: ContextKey,
        merge: MergePolicy,
    ) -> Self
    where
        T: DirectiveValue,
        for<'input, 'directive> T:
            TryFrom<&'input DirectiveInput<'directive>, Error = <T as DirectiveValue>::Error>,
    {
        Self {
            name,
            allowed_in,
            shape: DirectiveShape::ContextBlock {
                child_context,
                payload: PayloadMode::Parser,
            },
            parser: payload_value_parser::<T>,
            merge,
        }
    }
}

fn slot_value_parser<T>(
    input: &DirectiveInput<'_>,
) -> Result<ParsedDirective, Box<dyn std::error::Error + Send + Sync>>
where
    T: DirectiveValue,
    for<'input, 'directive> T:
        TryFrom<&'input DirectiveInput<'directive>, Error = <T as DirectiveValue>::Error>,
{
    let span = T::span(input);
    match T::try_from(input) {
        Ok(value) => Ok(ParsedDirective::Slot(TypedValue::new(value, span))),
        Err(source) => Err(Box::new(source)),
    }
}

fn payload_value_parser<T>(
    input: &DirectiveInput<'_>,
) -> Result<ParsedDirective, Box<dyn std::error::Error + Send + Sync>>
where
    T: DirectiveValue,
    for<'input, 'directive> T:
        TryFrom<&'input DirectiveInput<'directive>, Error = <T as DirectiveValue>::Error>,
{
    let span = T::span(input);
    match T::try_from(input) {
        Ok(value) => Ok(ParsedDirective::Payload(TypedValue::new(value, span))),
        Err(source) => Err(Box::new(source)),
    }
}

fn empty_parser(
    _input: &DirectiveInput<'_>,
) -> Result<ParsedDirective, Box<dyn std::error::Error + Send + Sync>> {
    Ok(ParsedDirective::Empty)
}

#[derive(Debug, Clone, Copy, Default)]
pub struct BuildOptions<'a> {
    pub identity_profile: Option<&'a dhttp::home::identity::IdentityProfile>,
}

impl ConfigRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_context(&mut self, spec: ContextSpec) {
        self.contexts.insert(spec.key, spec);
    }

    pub fn register_directive(&mut self, context: ContextKey, spec: DirectiveSpec) {
        self.directives.insert((context, spec.name), spec);
    }

    pub fn build(
        &self,
        source_map: Arc<SourceMap>,
        directives: Vec<AstDirective>,
        options: BuildOptions<'_>,
    ) -> Result<ConfigDocument, BuildDocumentError> {
        let span = directives
            .first()
            .map(|directive| directive.span)
            .unwrap_or(SourceSpan {
                source_id: crate::parse::source::SourceId(0),
                start: 0,
                end: 0,
            });
        let mut root = ConfigNode::new(context::ROOT, None, span);
        self.build_into(
            source_map.as_ref(),
            &mut root,
            context::ROOT,
            directives,
            &options,
        )?;
        let root = Arc::new(root);
        put_parent_recursively(&root, None);
        Ok(ConfigDocument::new(source_map, root))
    }

    fn build_into(
        &self,
        source_map: &SourceMap,
        node: &mut ConfigNode,
        context: ContextKey,
        directives: Vec<AstDirective>,
        options: &BuildOptions<'_>,
    ) -> Result<(), BuildDocumentError> {
        for directive in directives {
            let directive_name = directive.name.value.clone();
            let spec = self
                .directives
                .get(&(context, directive_name.as_str()))
                .with_context(|| build_document_error::UnknownDirectiveSnafu {
                    directive: directive_name.clone(),
                    span: directive.name.span,
                })?;
            apply_shape(&directive, spec)?;
            let parsed = (spec.parser)(&DirectiveInput {
                directive: &directive,
                context,
                source_map,
            })
            .context(build_document_error::DirectiveParseSnafu {
                directive: directive_name.clone(),
                span: directive.span,
            })?;
            match spec.shape {
                DirectiveShape::Leaf | DirectiveShape::RawBlock => {
                    if let ParsedDirective::Slot(value) = parsed {
                        insert_slot(node, spec, value, source_map)?;
                    }
                }
                DirectiveShape::ContextBlock {
                    child_context,
                    payload,
                } => {
                    let mut child = ConfigNode::new(
                        child_context,
                        Some(directive.name.clone()),
                        directive.span,
                    );
                    if matches!(payload, PayloadMode::Parser)
                        && let ParsedDirective::Payload(value) = parsed
                    {
                        child.set_payload(value);
                    }
                    let AstBody::Block { children, .. } = directive.body else {
                        unreachable!("shape checked block before child build");
                    };
                    self.build_into(source_map, &mut child, child_context, children, options)?;
                    self.finalize(&mut child, child_context, options)?;
                    node.insert_child(spec.name, Arc::new(child));
                }
            }
        }
        self.finalize(node, context, options)?;
        Ok(())
    }

    fn finalize(
        &self,
        node: &mut ConfigNode,
        context: ContextKey,
        options: &BuildOptions<'_>,
    ) -> Result<(), BuildDocumentError> {
        if let Some(spec) = self.contexts.get(&context)
            && let Some(finalize) = spec.finalize
        {
            finalize(node, options).context(build_document_error::FinalizeContextSnafu {
                context: context.0,
                span: node.span,
            })?;
        }
        Ok(())
    }
}

fn apply_shape(directive: &AstDirective, spec: &DirectiveSpec) -> Result<(), BuildDocumentError> {
    match (&directive.body, spec.shape) {
        (AstBody::Leaf { .. }, DirectiveShape::Leaf | DirectiveShape::RawBlock) => Ok(()),
        (AstBody::Block { .. }, DirectiveShape::ContextBlock { .. } | DirectiveShape::RawBlock) => {
            Ok(())
        }
        _ => build_document_error::InvalidDirectiveShapeSnafu {
            directive: directive.name.value.clone(),
            span: directive.span,
        }
        .fail(),
    }
}

fn insert_slot(
    node: &mut ConfigNode,
    spec: &DirectiveSpec,
    value: TypedValue,
    source_map: &SourceMap,
) -> Result<(), BuildDocumentError> {
    let span = value.span();
    let value = normalize::normalize_slot_value(value, source_map).context(
        build_document_error::NormalizeDirectiveValueSnafu {
            directive: spec.name.to_owned(),
            span,
        },
    )?;

    match spec.merge {
        MergePolicy::RejectDuplicate if !node.get_all_untyped(spec.name).is_empty() => {
            let first = node.get_all_untyped(spec.name)[0].span();
            build_document_error::DuplicateDirectiveSnafu {
                directive: spec.name.to_owned(),
                first,
                duplicate: value.span(),
            }
            .fail()
        }
        MergePolicy::LastWins => {
            node.replace_slot(spec.name, value);
            Ok(())
        }
        _ => {
            node.insert_slot(spec.name, value);
            Ok(())
        }
    }
}

fn put_parent_recursively(node: &Arc<ConfigNode>, parent: Option<&Arc<ConfigNode>>) {
    node.set_parent(parent.map(Arc::downgrade));
    for children in node.child_groups() {
        for child in children {
            put_parent_recursively(child, Some(node));
        }
    }
}
