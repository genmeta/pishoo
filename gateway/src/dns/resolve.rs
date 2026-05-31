use std::sync::Arc;

use ddns::resolvers::{Resolvers, h3::H3Resolver};
use h3x::{
    dquic::{
        prelude::{Connection, QuicClient},
        qresolve::{Resolve as GmdnsResolver, SystemResolver},
    },
    endpoint::H3Endpoint,
};
use http::Uri;

use super::H3_DNS_SERVER;
use crate::parse::{
    document::ConfigNode,
    types::{
        ResolverConfig, ServerIdentity, ServerNames, optional_server_identity, server_identity,
    },
};

type DnsH3Client = H3Endpoint<Arc<QuicClient>, Connection>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnsResolver {
    pub base_url: Uri,
}

impl DnsResolver {
    pub fn default_h3() -> Self {
        Self {
            base_url: H3_DNS_SERVER.parse().expect("invalid h3 dns server"),
        }
    }

    pub fn from_node_or_default(node: &ConfigNode) -> Self {
        node.get::<ResolverConfig>("dns")
            .ok()
            .flatten()
            .map(|resolver| Self {
                base_url: resolver.0.clone(),
            })
            .unwrap_or_else(Self::default_h3)
    }

    pub fn build_query_resolver(
        &self,
        config: Option<&ServerIdentity>,
    ) -> Arc<dyn GmdnsResolver + Send + Sync> {
        let client = if let Some(config) = config {
            self.create_h3_client(config)
        } else {
            self.create_h3_client_no_auth()
        };

        Arc::new(
            H3Resolver::new(self.base_url.to_string(), client)
                .expect("h3 dns server base_url has been checked"),
        )
    }

    pub(crate) fn create_h3_client_no_auth(&self) -> DnsH3Client {
        let root_store = crate::common::root_cert();
        let quic = QuicClient::builder()
            .with_root_certificates(root_store)
            .without_cert()
            .with_alpns(vec!["h3"])
            .build();
        H3Endpoint::new(Arc::new(quic))
    }

    pub(crate) fn create_h3_client(&self, config: &ServerIdentity) -> DnsH3Client {
        let root_store = crate::common::root_cert();

        let (cert_path, key_path, name) =
            (&config.cert_path, &config.key_path, &config.server_name);
        let (Ok(cert_data), Ok(key_data)) = (std::fs::read(cert_path), std::fs::read(key_path))
        else {
            return self.create_h3_client_no_auth();
        };

        use std::io::Cursor;

        use rustls_pemfile::{certs, private_key};

        let cert_chain: Vec<_> = certs(&mut Cursor::new(&cert_data))
            .collect::<Result<Vec<_>, _>>()
            .expect("failed to parse certificates");

        let private_key = private_key(&mut Cursor::new(&key_data))
            .expect("failed to parse private key")
            .expect("no private key found");

        let quic = QuicClient::builder()
            .with_root_certificates(root_store)
            .with_name(name.as_full().to_owned())
            .with_cert(cert_chain, private_key)
            .with_alpns(vec!["h3"])
            .build();
        H3Endpoint::new(Arc::new(quic))
    }
}

pub fn build_query_resolvers(node: &ConfigNode, server_name_key: &str) -> Resolvers {
    let resolver = DnsResolver::from_node_or_default(node);
    let identity = optional_server_identity(node, server_name_key);

    Resolvers::default()
        .with(resolver.build_query_resolver(identity.as_ref()))
        .with(Arc::new(SystemResolver))
}

pub fn build_query_resolver_chain(servers: &[Arc<ConfigNode>]) -> Resolvers {
    let mut resolvers = Resolvers::default();
    let mut seen = std::collections::HashSet::new();

    for server in servers {
        let resolver = DnsResolver::from_node_or_default(server);
        let server_names = server
            .require::<ServerNames>("server_name")
            .expect("server_name is required for server config");

        for server_name in &server_names.0 {
            let domain = server_name.name.clone();
            let resolver_key = (resolver.base_url.to_string(), domain.clone());
            if seen.insert(resolver_key) {
                let identity = server_identity(server, domain.clone())
                    .expect("missing ssl_certificate or ssl_certificate_key");
                resolvers = resolvers.with(resolver.build_query_resolver(Some(&identity)));
            }
        }
    }

    if seen.is_empty() {
        resolvers = resolvers.with(DnsResolver::default_h3().build_query_resolver(None));
    }

    resolvers.with(Arc::new(SystemResolver))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::{
        document::ConfigNode,
        registry::context,
        source::{SourceId, SourceSpan},
        types::ResolverConfig,
        value::TypedValue,
    };

    fn test_node() -> ConfigNode {
        ConfigNode::new(context::SERVER, None, SourceSpan::new(SourceId(0), 0, 0))
    }

    #[test]
    fn test_from_node_or_default_reads_dns_directive() {
        let base_url: Uri = "https://dns.example.com/dns-query"
            .parse()
            .expect("valid uri");
        let mut node = test_node();
        node.insert_slot(
            "dns",
            TypedValue::new(ResolverConfig(base_url.clone()), node.span),
        );

        let resolver = DnsResolver::from_node_or_default(&node);

        assert_eq!(resolver.base_url, base_url);
    }

    #[test]
    fn test_from_node_or_default_falls_back_to_default_h3() {
        let node = test_node();

        let resolver = DnsResolver::from_node_or_default(&node);

        assert_eq!(resolver, DnsResolver::default_h3());
    }
}
