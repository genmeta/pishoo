use std::{num::NonZeroU32, path::Path, sync::Arc};

use snafu::Snafu;

use crate::parse::{
    cascade::{CascadedValue, ConfigOrigin, DirectiveKey, InheritedSourceLocation},
    document::ConfigNode,
    domain::{
        ConfigDocumentId, ConfigDocumentIdAllocator, ConfigDocumentIdError, ConfigSourceSpan,
        DirectiveName,
    },
    error::ConfigQueryError,
    fragment::{ParsedPishooFragment, ParsedServerFragment},
    registry::{
        CascadePolicy, ConfigRegistry, V1SnapshotSchemaError, ValidatedV1SnapshotSchema, context,
    },
    snapshot::{RootConfigSnapshot, RootConfigSnapshotError},
    source::{ConfigDocumentSourceMap, SourceId, SourceMap, SourceSpan},
    value::ConfigValue,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConfigNodeId(usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParentLink {
    Root,
    Node(ConfigNodeId),
}

#[derive(Debug)]
struct SealedConfigNode {
    document_id: Option<ConfigDocumentId>,
    node: Arc<ConfigNode>,
    parent: ParentLink,
    children: Vec<ConfigNodeId>,
    cascade_policies: Box<[(DirectiveName, CascadePolicy)]>,
}

#[derive(Clone, Copy)]
pub(crate) struct AttachedConfigNode<'tree> {
    nodes: &'tree [SealedConfigNode],
    node: ConfigNodeId,
}

impl<'tree> AttachedConfigNode<'tree> {
    fn new(nodes: &'tree [SealedConfigNode], node: ConfigNodeId) -> Self {
        Self { nodes, node }
    }

    pub(crate) fn context(self) -> crate::parse::registry::ContextKey {
        self.nodes[self.node.0].node.context
    }

    pub(crate) fn config(self) -> &'tree ConfigNode {
        &self.nodes[self.node.0].node
    }

    pub(crate) fn parent(self) -> Option<Self> {
        match self.nodes[self.node.0].parent {
            ParentLink::Root => None,
            ParentLink::Node(parent) => Some(Self::new(self.nodes, parent)),
        }
    }

    pub(crate) fn children(self) -> impl Iterator<Item = Self> + 'tree {
        self.nodes[self.node.0]
            .children
            .iter()
            .copied()
            .map(|node| Self::new(self.nodes, node))
    }
}

#[derive(Debug)]
struct ConfigSourceOwner {
    document_id: ConfigDocumentId,
    sources: Arc<ConfigDocumentSourceMap>,
}

impl ConfigSourceOwner {
    fn source_map(&self) -> &SourceMap {
        self.sources.source_map()
    }
}

#[derive(Debug)]
pub struct HomeConfigTree {
    nodes: Box<[SealedConfigNode]>,
    root: ConfigNodeId,
    pishoo: ConfigNodeId,
    servers: Box<[ConfigNodeId]>,
    inherited_root: Option<Arc<RootConfigSnapshot>>,
    sources: ConfigSourceBundle,
    snapshot_schema: ValidatedV1SnapshotSchema,
}

#[derive(Debug)]
pub struct ConfigSourceBundle {
    owners: Box<[ConfigSourceOwner]>,
}

#[derive(Debug, Clone)]
pub struct ConfigNodeRef {
    tree: Arc<HomeConfigTree>,
    node: ConfigNodeId,
}

#[derive(Debug, Clone)]
pub struct ServerConfigRef(ConfigNodeRef);

#[derive(Debug, Clone)]
pub struct LocationConfigRef(ConfigNodeRef);

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum HomeConfigTreeError {
    #[snafu(display("failed to allocate a sealed configuration document identity"))]
    DocumentId { source: ConfigDocumentIdError },
    #[snafu(display("non-root configuration node is missing its semantic parent"))]
    MissingParent { node: ConfigNodeId },
    #[snafu(display("sealed configuration node has an invalid semantic parent"))]
    InvalidParent { node: ConfigNodeId },
    #[snafu(display("sealed configuration node has an invalid context"))]
    InvalidContext { node: ConfigNodeId },
    #[snafu(display("sealed configuration node has no owning source document"))]
    MissingSource { node: ConfigNodeId },
    #[snafu(display("failed to finalize an attached configuration context"))]
    FinalizeAttached {
        node: ConfigNodeId,
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[snafu(display("registry is incompatible with the root snapshot schema"))]
    SnapshotContract { source: V1SnapshotSchemaError },
}

pub fn build_global_tree<I>(
    registry: &ConfigRegistry,
    root_fragment: ParsedPishooFragment,
    identity_fragments: I,
) -> Result<Arc<HomeConfigTree>, HomeConfigTreeError>
where
    I: IntoIterator<Item = ParsedServerFragment>,
{
    HomeConfigTreeBuilder::global(registry, root_fragment, identity_fragments.into_iter())?.seal()
}

pub fn build_worker_tree<I>(
    registry: &ConfigRegistry,
    root_snapshot: RootConfigSnapshot,
    worker_fragment: Option<ParsedPishooFragment>,
    identity_fragments: I,
) -> Result<Arc<HomeConfigTree>, HomeConfigTreeError>
where
    I: IntoIterator<Item = ParsedServerFragment>,
{
    HomeConfigTreeBuilder::worker(
        registry,
        root_snapshot,
        worker_fragment,
        identity_fragments.into_iter(),
    )?
    .seal()
}

struct HomeConfigTreeBuilder<'registry> {
    registry: &'registry ConfigRegistry,
    nodes: Vec<SealedConfigNode>,
    root: ConfigNodeId,
    pishoo: ConfigNodeId,
    servers: Vec<ConfigNodeId>,
    inherited_root: Option<Arc<RootConfigSnapshot>>,
    sources: Vec<ConfigSourceOwner>,
    document_ids: ConfigDocumentIdAllocator,
    snapshot_schema: ValidatedV1SnapshotSchema,
}

impl<'registry> HomeConfigTreeBuilder<'registry> {
    fn global(
        registry: &'registry ConfigRegistry,
        fragment: ParsedPishooFragment,
        identity_fragments: impl Iterator<Item = ParsedServerFragment>,
    ) -> Result<Self, HomeConfigTreeError> {
        let snapshot_schema = registry
            .validate_v1_snapshot_schema()
            .map_err(|source| HomeConfigTreeError::SnapshotContract { source })?;
        let synthetic_span = fragment.node().span;
        let root_node = Arc::new(ConfigNode::new(context::ROOT, None, synthetic_span));
        let mut builder = Self {
            registry,
            nodes: Vec::new(),
            root: ConfigNodeId(0),
            pishoo: ConfigNodeId(0),
            servers: Vec::new(),
            inherited_root: None,
            sources: Vec::new(),
            document_ids: ConfigDocumentIdAllocator::new(),
            snapshot_schema,
        };
        let document_id = builder.allocate_document_id()?;
        builder.root = builder.push_node(None, root_node, ParentLink::Root);
        builder.pishoo = builder.push_node(
            Some(document_id),
            Arc::clone(fragment.node()),
            ParentLink::Node(builder.root),
        );
        builder.nodes[builder.root.0].children.push(builder.pishoo);
        for server in fragment.servers() {
            builder.attach_server(server, document_id);
        }
        let sources = fragment.source_owner();
        builder.sources.push(ConfigSourceOwner {
            document_id,
            sources,
        });
        for fragment in identity_fragments {
            builder.attach_identity_fragment(fragment)?;
        }
        Ok(builder)
    }

    fn worker(
        registry: &'registry ConfigRegistry,
        root_snapshot: RootConfigSnapshot,
        worker_fragment: Option<ParsedPishooFragment>,
        identity_fragments: impl Iterator<Item = ParsedServerFragment>,
    ) -> Result<Self, HomeConfigTreeError> {
        let snapshot_schema = registry
            .validate_v1_snapshot_schema()
            .map_err(|source| HomeConfigTreeError::SnapshotContract { source })?;
        let synthetic_span = worker_fragment
            .as_ref()
            .map_or_else(synthetic_span, |fragment| fragment.node().span);
        let root_node = Arc::new(ConfigNode::new(context::ROOT, None, synthetic_span));
        let pishoo_node = worker_fragment.as_ref().map_or_else(
            || Arc::new(ConfigNode::new(context::PISHOO, None, synthetic_span)),
            |fragment| Arc::clone(fragment.node()),
        );
        let mut builder = Self {
            registry,
            nodes: Vec::new(),
            root: ConfigNodeId(0),
            pishoo: ConfigNodeId(0),
            servers: Vec::new(),
            inherited_root: Some(Arc::new(root_snapshot)),
            sources: Vec::new(),
            document_ids: ConfigDocumentIdAllocator::new(),
            snapshot_schema,
        };
        builder.root = builder.push_node(None, root_node, ParentLink::Root);
        let worker_document_id = worker_fragment
            .as_ref()
            .map(|_| builder.allocate_document_id())
            .transpose()?;
        builder.pishoo = builder.push_node(
            worker_document_id,
            pishoo_node,
            ParentLink::Node(builder.root),
        );
        builder.nodes[builder.root.0].children.push(builder.pishoo);
        if let Some(fragment) = worker_fragment {
            let Some(document_id) = worker_document_id else {
                return Err(HomeConfigTreeError::MissingSource {
                    node: builder.pishoo,
                });
            };
            let sources = fragment.source_owner();
            builder.sources.push(ConfigSourceOwner {
                document_id,
                sources,
            });
        }
        for fragment in identity_fragments {
            builder.attach_identity_fragment(fragment)?;
        }
        Ok(builder)
    }

    fn attach_identity_fragment(
        &mut self,
        fragment: ParsedServerFragment,
    ) -> Result<(), HomeConfigTreeError> {
        if let Some(document_id) = self.document_id(fragment.source_map()) {
            self.attach_server(&fragment, document_id);
        } else {
            let document_id = self.allocate_document_id()?;
            self.attach_server(&fragment, document_id);
            let sources = fragment.source_owner();
            self.sources.push(ConfigSourceOwner {
                document_id,
                sources,
            });
        }
        Ok(())
    }

    fn attach_server(&mut self, fragment: &ParsedServerFragment, document_id: ConfigDocumentId) {
        let server = self.push_node(
            Some(document_id),
            Arc::clone(fragment.node()),
            ParentLink::Node(self.pishoo),
        );
        self.servers.push(server);
        for location in fragment.locations() {
            let location = self.push_node(
                Some(document_id),
                Arc::clone(location.node()),
                ParentLink::Node(server),
            );
            self.nodes[server.0].children.push(location);
        }
        self.nodes[self.pishoo.0].children.push(server);
    }

    fn allocate_document_id(&mut self) -> Result<ConfigDocumentId, HomeConfigTreeError> {
        self.document_ids
            .allocate()
            .map_err(|source| HomeConfigTreeError::DocumentId { source })
    }

    fn document_id(&self, source_map: &SourceMap) -> Option<ConfigDocumentId> {
        self.sources
            .iter()
            .find(|owner| std::ptr::eq(owner.source_map(), source_map))
            .map(|owner| owner.document_id)
    }

    fn push_node(
        &mut self,
        document_id: Option<ConfigDocumentId>,
        node: Arc<ConfigNode>,
        parent: ParentLink,
    ) -> ConfigNodeId {
        let id = ConfigNodeId(self.nodes.len());
        let cascade_policies = self.registry.cascade_policies(node.context);
        self.nodes.push(SealedConfigNode {
            document_id,
            node,
            parent,
            children: Vec::new(),
            cascade_policies,
        });
        id
    }

    fn finalize_attached(&self) -> Result<(), HomeConfigTreeError> {
        for (index, node) in self.nodes.iter().enumerate() {
            let id = ConfigNodeId(index);
            if id != self.root && !matches!(node.parent, ParentLink::Node(_)) {
                return Err(HomeConfigTreeError::MissingParent { node: id });
            }
            if node.document_id.is_some_and(|document_id| {
                !self
                    .sources
                    .iter()
                    .any(|source| source.document_id == document_id)
            }) {
                return Err(HomeConfigTreeError::MissingSource { node: id });
            }
        }

        self.verify_node(self.root, context::ROOT, ParentLink::Root)?;
        self.verify_node(self.pishoo, context::PISHOO, ParentLink::Node(self.root))?;
        if self.nodes[self.root.0].children.as_slice() != [self.pishoo] {
            return Err(HomeConfigTreeError::InvalidParent { node: self.pishoo });
        }
        if self.nodes[self.pishoo.0].children.as_slice() != self.servers.as_slice() {
            return Err(HomeConfigTreeError::InvalidParent { node: self.pishoo });
        }
        for &server in &self.servers {
            self.verify_node(server, context::SERVER, ParentLink::Node(self.pishoo))?;
            for &location in &self.nodes[server.0].children {
                self.verify_node(location, context::LOCATION, ParentLink::Node(server))?;
            }
        }
        for index in 0..self.nodes.len() {
            let node = ConfigNodeId(index);
            self.registry
                .finalize_attached(AttachedConfigNode::new(&self.nodes, node))
                .map_err(|source| HomeConfigTreeError::FinalizeAttached { node, source })?;
        }
        Ok(())
    }

    fn verify_node(
        &self,
        node: ConfigNodeId,
        context: crate::parse::registry::ContextKey,
        parent: ParentLink,
    ) -> Result<(), HomeConfigTreeError> {
        let sealed = &self.nodes[node.0];
        if sealed.node.context != context {
            return Err(HomeConfigTreeError::InvalidContext { node });
        }
        if sealed.parent != parent {
            return Err(HomeConfigTreeError::InvalidParent { node });
        }
        Ok(())
    }

    fn seal(self) -> Result<Arc<HomeConfigTree>, HomeConfigTreeError> {
        self.finalize_attached()?;
        Ok(Arc::new(HomeConfigTree {
            nodes: self.nodes.into_boxed_slice(),
            root: self.root,
            pishoo: self.pishoo,
            servers: self.servers.into_boxed_slice(),
            inherited_root: self.inherited_root,
            sources: ConfigSourceBundle {
                owners: self.sources.into_boxed_slice(),
            },
            snapshot_schema: self.snapshot_schema,
        }))
    }
}

fn synthetic_span() -> SourceSpan {
    SourceSpan::new(SourceId(0), 0, 0)
}

impl HomeConfigTree {
    pub fn root(self: &Arc<Self>) -> ConfigNodeRef {
        ConfigNodeRef::new(Arc::clone(self), self.root)
    }

    pub fn pishoo(self: &Arc<Self>) -> ConfigNodeRef {
        ConfigNodeRef::new(Arc::clone(self), self.pishoo)
    }

    pub fn servers(self: &Arc<Self>) -> impl Iterator<Item = ServerConfigRef> + '_ {
        self.servers
            .iter()
            .copied()
            .map(|node| ServerConfigRef(ConfigNodeRef::new(Arc::clone(self), node)))
    }

    pub fn root_snapshot(self: &Arc<Self>) -> Result<RootConfigSnapshot, RootConfigSnapshotError> {
        RootConfigSnapshot::project(self)
    }

    pub(crate) const fn v1_snapshot_schema(&self) -> ValidatedV1SnapshotSchema {
        self.snapshot_schema
    }

    pub fn source_path(&self, span: ConfigSourceSpan) -> Option<&Path> {
        self.sources.source_path(span)
    }

    pub fn sources(&self) -> &ConfigSourceBundle {
        &self.sources
    }

    pub(crate) fn inherited_source_location(
        &self,
        span: ConfigSourceSpan,
    ) -> Result<InheritedSourceLocation, RootConfigSnapshotError> {
        self.sources.inherited_source_location(span)
    }

    fn node(&self, id: ConfigNodeId) -> &SealedConfigNode {
        &self.nodes[id.0]
    }

    fn cascaded<T>(
        &self,
        node: ConfigNodeId,
        key: DirectiveKey<T>,
    ) -> Result<Option<CascadedValue<Arc<T>>>, ConfigQueryError>
    where
        T: ConfigValue,
    {
        let mut chain = Vec::new();
        let mut current = node;
        loop {
            chain.push(current);
            match self.node(current).parent {
                ParentLink::Root => break,
                ParentLink::Node(parent) => current = parent,
            }
        }
        chain.reverse();
        let policy = self.cascade_policy(&chain, key.name())?;
        if !matches!(
            policy,
            CascadePolicy::NearestWins | CascadePolicy::ReplaceWhole
        ) {
            return Err(ConfigQueryError::UnsupportedCascadePolicy {
                directive: key.name().as_str().to_owned(),
                policy,
            });
        }

        let mut lineage = Vec::new();
        let mut effective = key.builtin();
        if effective.is_some() {
            lineage.push(ConfigOrigin::Builtin {
                directive: key.name(),
            });
        }
        if let Some(snapshot) = &self.inherited_root
            && let Some((value, origin)) = key.snapshot(snapshot)
        {
            effective = Some(value);
            if !matches!(origin, ConfigOrigin::Builtin { .. }) {
                lineage.push(origin);
            }
        }

        for id in chain {
            let node = self.node(id);
            if let Some((value, span)) = node.node.get_with_span::<T>(key.name().as_str())? {
                effective = Some(value);
                let origin = node.document_id.map_or(
                    ConfigOrigin::Builtin {
                        directive: key.name(),
                    },
                    |document_id| ConfigOrigin::Source(ConfigSourceSpan::new(document_id, span)),
                );
                lineage.push(origin);
            }
        }
        Ok(effective.map(|effective| CascadedValue::new(effective, lineage.into_boxed_slice())))
    }

    fn cascade_policy(
        &self,
        chain: &[ConfigNodeId],
        directive: DirectiveName,
    ) -> Result<CascadePolicy, ConfigQueryError> {
        let mut inherited = None;
        for &node in chain {
            let Some(local) = self.node(node).cascade_policy(directive) else {
                continue;
            };
            if let Some(inherited) = inherited
                && inherited != local
            {
                return Err(ConfigQueryError::CascadePolicyMismatch {
                    directive: directive.as_str().to_owned(),
                    inherited,
                    local,
                });
            }
            inherited = Some(local);
        }
        inherited.ok_or_else(|| ConfigQueryError::MissingCascadePolicy {
            directive: directive.as_str().to_owned(),
        })
    }
}

impl SealedConfigNode {
    fn cascade_policy(&self, directive: DirectiveName) -> Option<CascadePolicy> {
        self.cascade_policies
            .iter()
            .find_map(|(name, policy)| (*name == directive).then_some(*policy))
    }
}

impl ConfigSourceBundle {
    pub fn source_path(&self, span: ConfigSourceSpan) -> Option<&Path> {
        self.source_map(span.document_id())
            .and_then(|sources| sources.get(span.source_span().source_id))
            .and_then(|source| source.path.as_deref())
    }

    fn inherited_source_location(
        &self,
        span: ConfigSourceSpan,
    ) -> Result<InheritedSourceLocation, RootConfigSnapshotError> {
        let source_map = self
            .source_map(span.document_id())
            .ok_or(RootConfigSnapshotError::MissingSourceLocation)?;
        let source_span = span.source_span();
        let source = source_map
            .get(source_span.source_id)
            .ok_or(RootConfigSnapshotError::MissingSourceLocation)?;
        if let Some(path) = &source.path
            && (!path.is_absolute() || path.as_os_str().as_encoded_bytes().contains(&0))
        {
            return Err(RootConfigSnapshotError::SourcePath { path: path.clone() });
        }
        let location = source_map
            .line_column(source_span)
            .ok_or(RootConfigSnapshotError::MissingSourceLocation)?;
        let line = u32::try_from(location.line)
            .ok()
            .and_then(NonZeroU32::new)
            .ok_or(RootConfigSnapshotError::SourceLocation {
                line: location.line,
                column: location.column,
            })?;
        let column = u32::try_from(location.column)
            .ok()
            .and_then(NonZeroU32::new)
            .ok_or(RootConfigSnapshotError::SourceLocation {
                line: location.line,
                column: location.column,
            })?;
        Ok(InheritedSourceLocation::new(
            source.path.clone(),
            line,
            column,
        ))
    }

    fn source_map(&self, document_id: ConfigDocumentId) -> Option<&SourceMap> {
        self.owners
            .iter()
            .find(|owner| owner.document_id == document_id)
            .map(ConfigSourceOwner::source_map)
    }
}

impl ConfigNodeRef {
    fn new(tree: Arc<HomeConfigTree>, node: ConfigNodeId) -> Self {
        Self { tree, node }
    }

    pub const fn id(&self) -> ConfigNodeId {
        self.node
    }

    pub fn parent_link(&self) -> ParentLink {
        self.tree.node(self.node).parent
    }

    pub fn source_span(&self) -> Option<ConfigSourceSpan> {
        let node = self.tree.node(self.node);
        node.document_id
            .map(|document_id| ConfigSourceSpan::new(document_id, node.node.span))
    }

    pub fn tree(&self) -> &Arc<HomeConfigTree> {
        &self.tree
    }

    pub fn cascaded<T>(
        &self,
        key: DirectiveKey<T>,
    ) -> Result<Option<CascadedValue<Arc<T>>>, ConfigQueryError>
    where
        T: ConfigValue,
    {
        self.tree.cascaded(self.node, key)
    }
}

impl ServerConfigRef {
    pub fn node(&self) -> &ConfigNodeRef {
        &self.0
    }

    pub fn tree(&self) -> &Arc<HomeConfigTree> {
        self.0.tree()
    }

    pub fn locations(&self) -> impl Iterator<Item = LocationConfigRef> + '_ {
        self.0
            .tree
            .node(self.0.node)
            .children
            .iter()
            .copied()
            .map(|node| LocationConfigRef(ConfigNodeRef::new(Arc::clone(&self.0.tree), node)))
    }
}

impl LocationConfigRef {
    pub fn node(&self) -> &ConfigNodeRef {
        &self.0
    }

    pub fn tree(&self) -> &Arc<HomeConfigTree> {
        self.0.tree()
    }
}
