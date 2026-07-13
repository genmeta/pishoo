use std::sync::Arc;

use crate::parse::{
    document::ConfigNode,
    domain::{ConfigDocumentId, ConfigSourceSpan},
    source::SourceMap,
};

#[derive(Debug)]
pub enum ParsedConfigDocument {
    HypervisorRoot(ParsedPishooFragment),
    WorkerPishoo(ParsedPishooFragment),
    IdentityServers(Box<[ParsedServerFragment]>),
}

#[derive(Debug)]
pub struct ParsedPishooFragment {
    document_id: ConfigDocumentId,
    source_map: Arc<SourceMap>,
    node: Arc<ConfigNode>,
    servers: Box<[ParsedServerFragment]>,
}

#[derive(Debug)]
pub struct ParsedServerFragment {
    document_id: ConfigDocumentId,
    source_map: Arc<SourceMap>,
    node: Arc<ConfigNode>,
    locations: Box<[ParsedLocationFragment]>,
}

#[derive(Debug)]
pub struct ParsedLocationFragment {
    document_id: ConfigDocumentId,
    source_map: Arc<SourceMap>,
    node: Arc<ConfigNode>,
}

impl ParsedPishooFragment {
    pub(crate) fn new(source_map: Arc<SourceMap>, node: Arc<ConfigNode>) -> Self {
        let document_id = source_map.document_id();
        let servers = node
            .children_optional("server")
            .iter()
            .cloned()
            .map(|server| ParsedServerFragment::new(Arc::clone(&source_map), server))
            .collect();
        Self {
            document_id,
            source_map,
            node,
            servers,
        }
    }

    pub fn document_id(&self) -> ConfigDocumentId {
        self.document_id
    }

    pub fn span(&self) -> ConfigSourceSpan {
        self.source_map.config_span(self.node.span)
    }

    pub fn servers(&self) -> &[ParsedServerFragment] {
        &self.servers
    }

    #[allow(dead_code)] // consumed by the detached-to-sealed tree builder in the next parser stage
    pub(crate) fn source_map(&self) -> &SourceMap {
        &self.source_map
    }

    #[allow(dead_code)] // consumed by the detached-to-sealed tree builder in the next parser stage
    pub(crate) fn node(&self) -> &Arc<ConfigNode> {
        &self.node
    }
}

impl ParsedServerFragment {
    pub(crate) fn new(source_map: Arc<SourceMap>, node: Arc<ConfigNode>) -> Self {
        let document_id = source_map.document_id();
        let locations = node
            .children_optional("location")
            .iter()
            .cloned()
            .map(|location| ParsedLocationFragment::new(Arc::clone(&source_map), location))
            .collect();
        Self {
            document_id,
            source_map,
            node,
            locations,
        }
    }

    pub fn document_id(&self) -> ConfigDocumentId {
        self.document_id
    }

    pub fn span(&self) -> ConfigSourceSpan {
        self.source_map.config_span(self.node.span)
    }

    pub fn locations(&self) -> &[ParsedLocationFragment] {
        &self.locations
    }

    #[allow(dead_code)] // consumed by the detached-to-sealed tree builder in the next parser stage
    pub(crate) fn source_map(&self) -> &SourceMap {
        &self.source_map
    }

    #[allow(dead_code)] // consumed by the detached-to-sealed tree builder in the next parser stage
    pub(crate) fn node(&self) -> &Arc<ConfigNode> {
        &self.node
    }
}

impl ParsedLocationFragment {
    fn new(source_map: Arc<SourceMap>, node: Arc<ConfigNode>) -> Self {
        Self {
            document_id: source_map.document_id(),
            source_map,
            node,
        }
    }

    pub fn document_id(&self) -> ConfigDocumentId {
        self.document_id
    }

    pub fn span(&self) -> ConfigSourceSpan {
        self.source_map.config_span(self.node.span)
    }

    #[allow(dead_code)] // consumed by the detached-to-sealed tree builder in the next parser stage
    pub(crate) fn source_map(&self) -> &SourceMap {
        &self.source_map
    }

    #[allow(dead_code)] // consumed by the detached-to-sealed tree builder in the next parser stage
    pub(crate) fn node(&self) -> &Arc<ConfigNode> {
        &self.node
    }
}
