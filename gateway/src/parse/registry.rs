use std::{collections::HashMap, error::Error, sync::Arc};

use snafu::{OptionExt, ResultExt};

use crate::parse::{
    ast::{AstBody, AstDirective},
    document::{ConfigDocument, ConfigNode},
    domain::{ConfigDocumentRoleKind, DirectiveName},
    error::{BuildDocumentError, ConfigDocumentRoleError, build_document_error},
    fragment::{ParsedConfigDocument, ParsedPishooFragment, ParsedServerFragment},
    normalize,
    source::{ConfigDocumentSourceMap, SourceMap, SourceSpan},
    value::{ConfigValue, TypedValue},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContextKey(pub &'static str);

impl std::fmt::Display for ContextKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

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
    pub name: DirectiveName,
    pub allowed_in: Vec<ContextKey>,
    pub shape: DirectiveShape,
    parser: DirectiveParserFn,
    pub duplicate: DuplicatePolicy,
    pub cascade: CascadePolicy,
    pub transport: TransportPolicy,
    pub reload: ReloadImpact,
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
pub enum DuplicatePolicy {
    Reject,
    LastWins,
    Append,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CascadePolicy {
    None,
    NearestWins,
    ReplaceWhole,
    MergeByKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportPolicy {
    HypervisorOnly,
    WorkerInheritable,
    WorkerLocalOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadImpact {
    Supervisor,
    ListenerSet,
    RuntimeState,
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
        duplicate: DuplicatePolicy,
        cascade: CascadePolicy,
        transport: TransportPolicy,
        reload: ReloadImpact,
    ) -> Self
    where
        T: DirectiveValue,
        for<'input, 'directive> T:
            TryFrom<&'input DirectiveInput<'directive>, Error = <T as DirectiveValue>::Error>,
    {
        Self {
            name: DirectiveName::new(name),
            allowed_in,
            shape: DirectiveShape::Leaf,
            parser: slot_value_parser::<T>,
            duplicate,
            cascade,
            transport,
            reload,
        }
    }

    pub fn raw_value<T>(
        name: &'static str,
        allowed_in: Vec<ContextKey>,
        duplicate: DuplicatePolicy,
        cascade: CascadePolicy,
        transport: TransportPolicy,
        reload: ReloadImpact,
    ) -> Self
    where
        T: DirectiveValue,
        for<'input, 'directive> T:
            TryFrom<&'input DirectiveInput<'directive>, Error = <T as DirectiveValue>::Error>,
    {
        Self {
            name: DirectiveName::new(name),
            allowed_in,
            shape: DirectiveShape::RawBlock,
            parser: slot_value_parser::<T>,
            duplicate,
            cascade,
            transport,
            reload,
        }
    }

    pub fn context_empty(
        name: &'static str,
        allowed_in: Vec<ContextKey>,
        child_context: ContextKey,
        duplicate: DuplicatePolicy,
        cascade: CascadePolicy,
        transport: TransportPolicy,
        reload: ReloadImpact,
    ) -> Self {
        Self {
            name: DirectiveName::new(name),
            allowed_in,
            shape: DirectiveShape::ContextBlock {
                child_context,
                payload: PayloadMode::None,
            },
            parser: empty_parser,
            duplicate,
            cascade,
            transport,
            reload,
        }
    }

    pub fn context_payload<T>(
        name: &'static str,
        allowed_in: Vec<ContextKey>,
        child_context: ContextKey,
        duplicate: DuplicatePolicy,
        cascade: CascadePolicy,
        transport: TransportPolicy,
        reload: ReloadImpact,
    ) -> Self
    where
        T: DirectiveValue,
        for<'input, 'directive> T:
            TryFrom<&'input DirectiveInput<'directive>, Error = <T as DirectiveValue>::Error>,
    {
        Self {
            name: DirectiveName::new(name),
            allowed_in,
            shape: DirectiveShape::ContextBlock {
                child_context,
                payload: PayloadMode::Parser,
            },
            parser: payload_value_parser::<T>,
            duplicate,
            cascade,
            transport,
            reload,
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
    pub dhttp_home: Option<&'a dhttp::home::DhttpHome>,
    pub identity_profile: Option<&'a dhttp::home::identity::IdentityProfile>,
}

impl BuildOptions<'_> {
    pub fn has_dhttp_home_context(&self) -> bool {
        self.dhttp_home.is_some() || self.identity_profile.is_some()
    }
}

impl ConfigRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_context(&mut self, spec: ContextSpec) {
        self.contexts.insert(spec.key, spec);
    }

    pub fn register_directive(&mut self, context: ContextKey, spec: DirectiveSpec) {
        self.directives.insert((context, spec.name.as_str()), spec);
    }

    #[cfg(test)]
    pub(crate) fn directive_spec(&self, context: ContextKey, name: &str) -> Option<&DirectiveSpec> {
        self.directives
            .iter()
            .find_map(|((registered_context, registered_name), spec)| {
                (*registered_context == context && *registered_name == name).then_some(spec)
            })
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

    pub(crate) fn build_for_role(
        &self,
        sources: Arc<ConfigDocumentSourceMap>,
        directives: Vec<AstDirective>,
        options: BuildOptions<'_>,
        role: ConfigDocumentRoleKind,
    ) -> Result<ParsedConfigDocument, RoleDocumentBuildError> {
        self.validate_role_registration(&sources, &directives, role)?;
        validate_document_shape(&sources, &directives, role)?;
        self.validate_role_directives(&sources, &directives, context::ROOT, role)?;

        let span = document_span(&directives);
        let mut root = ConfigNode::new(context::ROOT, None, span);
        self.build_into(
            sources.source_map(),
            &mut root,
            context::ROOT,
            directives,
            &options,
        )
        .map_err(RoleDocumentBuildError::Build)?;

        match role {
            ConfigDocumentRoleKind::HypervisorRoot => {
                let pishoo = self.required_built_child(&sources, &root, "pishoo", role)?;
                Ok(ParsedConfigDocument::HypervisorRoot(
                    ParsedPishooFragment::new(sources, pishoo),
                ))
            }
            ConfigDocumentRoleKind::WorkerPishoo => {
                let pishoo = self.required_built_child(&sources, &root, "pishoo", role)?;
                Ok(ParsedConfigDocument::WorkerPishoo(
                    ParsedPishooFragment::new(sources, pishoo),
                ))
            }
            ConfigDocumentRoleKind::IdentityServer => {
                let servers: Box<[_]> = root
                    .children_optional("server")
                    .iter()
                    .cloned()
                    .map(|server| ParsedServerFragment::new(Arc::clone(&sources), server))
                    .collect();
                if servers.is_empty() {
                    return Err(RoleDocumentBuildError::Role(
                        ConfigDocumentRoleError::missing_built_directive(
                            DirectiveName::new("server"),
                            role,
                            sources.config_span(root.span),
                        ),
                    ));
                }
                Ok(ParsedConfigDocument::IdentityServers(servers))
            }
        }
    }

    fn validate_role_registration(
        &self,
        sources: &ConfigDocumentSourceMap,
        directives: &[AstDirective],
        role: ConfigDocumentRoleKind,
    ) -> Result<(), RoleDocumentBuildError> {
        let (directive, expected_child_context) = match role {
            ConfigDocumentRoleKind::HypervisorRoot | ConfigDocumentRoleKind::WorkerPishoo => {
                (DirectiveName::new("pishoo"), context::PISHOO)
            }
            ConfigDocumentRoleKind::IdentityServer => {
                (DirectiveName::new("server"), context::SERVER)
            }
        };
        let spec = self.directives.get(&(context::ROOT, directive.as_str()));
        let valid = spec.is_some_and(|spec| {
            spec.allowed_in.contains(&context::ROOT)
                && matches!(
                    spec.shape,
                    DirectiveShape::ContextBlock {
                        child_context,
                        payload: PayloadMode::None,
                    } if child_context == expected_child_context
                )
                && self.contexts.contains_key(&expected_child_context)
        });
        if valid {
            return Ok(());
        }
        let span = directives
            .iter()
            .find(|candidate| candidate.name.value == directive.as_str())
            .map_or_else(
                || document_span(directives),
                |candidate| candidate.name.span,
            );
        Err(RoleDocumentBuildError::Role(
            ConfigDocumentRoleError::invalid_directive_registration(
                directive,
                role,
                expected_child_context,
                sources.config_span(span),
            ),
        ))
    }

    fn required_built_child(
        &self,
        sources: &ConfigDocumentSourceMap,
        root: &ConfigNode,
        directive: &'static str,
        role: ConfigDocumentRoleKind,
    ) -> Result<Arc<ConfigNode>, RoleDocumentBuildError> {
        root.children_optional(directive)
            .first()
            .cloned()
            .ok_or_else(|| {
                RoleDocumentBuildError::Role(ConfigDocumentRoleError::missing_built_directive(
                    DirectiveName::new(directive),
                    role,
                    sources.config_span(root.span),
                ))
            })
    }

    fn validate_role_directives(
        &self,
        sources: &ConfigDocumentSourceMap,
        directives: &[AstDirective],
        context: ContextKey,
        role: ConfigDocumentRoleKind,
    ) -> Result<(), RoleDocumentBuildError> {
        for directive in directives {
            let Some(spec) = self
                .directives
                .get(&(context, directive.name.value.as_str()))
            else {
                continue;
            };
            if !directive_allowed_for_role(spec, context, role) {
                return Err(RoleDocumentBuildError::Role(
                    ConfigDocumentRoleError::directive_not_allowed(
                        spec.name,
                        role,
                        sources.config_span(directive.name.span),
                    ),
                ));
            }
            if let (
                DirectiveShape::ContextBlock { child_context, .. },
                AstBody::Block { children, .. },
            ) = (spec.shape, &directive.body)
            {
                self.validate_role_directives(sources, children, child_context, role)?;
            }
        }
        Ok(())
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
                    let children = match directive.body {
                        AstBody::Block { children, .. } => children,
                        AstBody::Leaf { .. } => {
                            return build_document_error::InvalidDirectiveShapeSnafu {
                                directive: directive_name,
                                span: directive.span,
                            }
                            .fail();
                        }
                    };
                    self.build_into(source_map, &mut child, child_context, children, options)?;
                    self.finalize(&mut child, child_context, options)?;
                    node.insert_child(spec.name.as_str(), Arc::new(child));
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
            directive: spec.name.as_str().to_owned(),
            span,
        },
    )?;

    match spec.duplicate {
        DuplicatePolicy::Reject if !node.get_all_untyped(spec.name.as_str()).is_empty() => {
            let first = node.get_all_untyped(spec.name.as_str())[0].span();
            build_document_error::DuplicateDirectiveSnafu {
                directive: spec.name.as_str().to_owned(),
                first,
                duplicate: value.span(),
            }
            .fail()
        }
        DuplicatePolicy::LastWins => {
            node.replace_slot(spec.name.as_str(), value);
            Ok(())
        }
        _ => {
            node.insert_slot(spec.name.as_str(), value);
            Ok(())
        }
    }
}

fn document_span(directives: &[AstDirective]) -> SourceSpan {
    directives
        .first()
        .map(|directive| directive.span)
        .unwrap_or(SourceSpan {
            source_id: crate::parse::source::SourceId(0),
            start: 0,
            end: 0,
        })
}

fn validate_document_shape(
    sources: &ConfigDocumentSourceMap,
    directives: &[AstDirective],
    role: ConfigDocumentRoleKind,
) -> Result<(), RoleDocumentBuildError> {
    match role {
        ConfigDocumentRoleKind::HypervisorRoot | ConfigDocumentRoleKind::WorkerPishoo => {
            let pishoo = directives
                .iter()
                .filter(|directive| directive.name.value == "pishoo")
                .collect::<Vec<_>>();
            if pishoo.len() != 1 {
                let span = pishoo
                    .get(1)
                    .copied()
                    .or_else(|| directives.first())
                    .map_or_else(|| document_span(directives), |directive| directive.span);
                return Err(RoleDocumentBuildError::Role(
                    ConfigDocumentRoleError::expected_single_pishoo(
                        role,
                        pishoo.len(),
                        sources.config_span(span),
                    ),
                ));
            }
        }
        ConfigDocumentRoleKind::IdentityServer => {
            if !directives
                .iter()
                .any(|directive| directive.name.value == "server")
            {
                return Err(RoleDocumentBuildError::Role(
                    ConfigDocumentRoleError::missing_identity_server(
                        role,
                        sources.config_span(document_span(directives)),
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn directive_allowed_for_role(
    spec: &DirectiveSpec,
    context: ContextKey,
    role: ConfigDocumentRoleKind,
) -> bool {
    match role {
        ConfigDocumentRoleKind::HypervisorRoot => {
            context != context::ROOT || spec.name.as_str() == "pishoo"
        }
        ConfigDocumentRoleKind::WorkerPishoo => spec.transport != TransportPolicy::HypervisorOnly,
        ConfigDocumentRoleKind::IdentityServer => {
            context != context::ROOT || spec.name.as_str() == "server"
        }
    }
}

pub(crate) enum RoleDocumentBuildError {
    Role(ConfigDocumentRoleError),
    Build(BuildDocumentError),
}

fn put_parent_recursively(node: &Arc<ConfigNode>, parent: Option<&Arc<ConfigNode>>) {
    node.set_parent(parent.map(Arc::downgrade));
    for children in node.child_groups() {
        for child in children {
            put_parent_recursively(child, Some(node));
        }
    }
}
