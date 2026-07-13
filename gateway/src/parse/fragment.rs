use std::sync::Arc;

use crate::parse::{
    document::ConfigNode,
    domain::{ConfigDocumentId, ConfigSourceSpan},
    registry::ConfigRegistryContract,
    source::{ConfigDocumentSourceMap, SourceMap},
};

#[derive(Debug)]
pub enum ParsedConfigDocument {
    HypervisorRoot(ParsedPishooFragment),
    WorkerPishoo(ParsedPishooFragment),
    IdentityServers(Box<[ParsedServerFragment]>),
}

#[derive(Debug)]
pub struct ParsedPishooFragment {
    sources: Arc<ConfigDocumentSourceMap>,
    node: Arc<ConfigNode>,
    servers: Box<[ParsedServerFragment]>,
    registry_contract: ConfigRegistryContract,
}

#[derive(Debug)]
pub struct ParsedServerFragment {
    sources: Arc<ConfigDocumentSourceMap>,
    node: Arc<ConfigNode>,
    locations: Box<[ParsedLocationFragment]>,
    registry_contract: ConfigRegistryContract,
}

#[derive(Debug)]
pub struct ParsedLocationFragment {
    sources: Arc<ConfigDocumentSourceMap>,
    node: Arc<ConfigNode>,
}

impl ParsedPishooFragment {
    pub(crate) fn new(
        sources: Arc<ConfigDocumentSourceMap>,
        node: Arc<ConfigNode>,
        registry_contract: ConfigRegistryContract,
    ) -> Self {
        let servers = node
            .children_optional("server")
            .iter()
            .cloned()
            .map(|server| {
                ParsedServerFragment::new(Arc::clone(&sources), server, registry_contract.clone())
            })
            .collect();
        Self {
            sources,
            node,
            servers,
            registry_contract,
        }
    }

    pub fn document_id(&self) -> ConfigDocumentId {
        self.sources.document_id()
    }

    pub fn span(&self) -> ConfigSourceSpan {
        self.sources.config_span(self.node.span)
    }

    pub fn servers(&self) -> &[ParsedServerFragment] {
        &self.servers
    }

    #[allow(dead_code)] // consumed by the detached-to-sealed tree builder in the next parser stage
    pub(crate) fn source_map(&self) -> &SourceMap {
        self.sources.source_map()
    }

    pub(crate) fn source_owner(&self) -> Arc<ConfigDocumentSourceMap> {
        Arc::clone(&self.sources)
    }

    pub(crate) fn registry_contract(&self) -> &ConfigRegistryContract {
        &self.registry_contract
    }

    #[allow(dead_code)] // consumed by the detached-to-sealed tree builder in the next parser stage
    pub(crate) fn node(&self) -> &Arc<ConfigNode> {
        &self.node
    }
}

impl ParsedServerFragment {
    pub(crate) fn new(
        sources: Arc<ConfigDocumentSourceMap>,
        node: Arc<ConfigNode>,
        registry_contract: ConfigRegistryContract,
    ) -> Self {
        let locations = node
            .children_optional("location")
            .iter()
            .cloned()
            .map(|location| ParsedLocationFragment::new(Arc::clone(&sources), location))
            .collect();
        Self {
            sources,
            node,
            locations,
            registry_contract,
        }
    }

    pub fn document_id(&self) -> ConfigDocumentId {
        self.sources.document_id()
    }

    pub fn span(&self) -> ConfigSourceSpan {
        self.sources.config_span(self.node.span)
    }

    pub fn locations(&self) -> &[ParsedLocationFragment] {
        &self.locations
    }

    #[allow(dead_code)] // consumed by the detached-to-sealed tree builder in the next parser stage
    pub(crate) fn source_map(&self) -> &SourceMap {
        self.sources.source_map()
    }

    pub(crate) fn source_owner(&self) -> Arc<ConfigDocumentSourceMap> {
        Arc::clone(&self.sources)
    }

    pub(crate) fn registry_contract(&self) -> &ConfigRegistryContract {
        &self.registry_contract
    }

    #[allow(dead_code)] // consumed by the detached-to-sealed tree builder in the next parser stage
    pub(crate) fn node(&self) -> &Arc<ConfigNode> {
        &self.node
    }
}

impl ParsedLocationFragment {
    fn new(sources: Arc<ConfigDocumentSourceMap>, node: Arc<ConfigNode>) -> Self {
        Self { sources, node }
    }

    pub fn document_id(&self) -> ConfigDocumentId {
        self.sources.document_id()
    }

    pub fn span(&self) -> ConfigSourceSpan {
        self.sources.config_span(self.node.span)
    }

    #[allow(dead_code)] // consumed by the detached-to-sealed tree builder in the next parser stage
    pub(crate) fn source_map(&self) -> &SourceMap {
        self.sources.source_map()
    }

    #[allow(dead_code)] // consumed by the detached-to-sealed tree builder in the next parser stage
    pub(crate) fn node(&self) -> &Arc<ConfigNode> {
        &self.node
    }
}
