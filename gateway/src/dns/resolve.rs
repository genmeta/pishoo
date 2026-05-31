use std::sync::Arc;

use ddns::resolvers::Resolvers;
use dhttp::{ddns::resolvers::DnsScheme, endpoint::Endpoint, identity::Identity};
use h3x::dquic::qresolve::Resolve as GmdnsResolver;
use http::Uri;

use super::H3_DNS_SERVER;
use crate::parse::{
    document::ConfigNode,
    types::{
        ResolverConfig, ServerIdentity, ServerNames, optional_server_identity, server_identity,
    },
};

type DnsH3Endpoint = Arc<dhttp::h3x::dquic::H3Endpoint>;

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

    pub async fn build_query_resolver(
        &self,
        config: Option<&ServerIdentity>,
    ) -> Arc<dyn GmdnsResolver + Send + Sync> {
        let endpoint = self.create_h3_endpoint(config).await;
        Arc::new(
            Resolvers::builder()
                .h3_with_base_url(self.base_url.to_string(), endpoint)
                .expect("h3 dns server base_url has been checked")
                .build(),
        )
    }

    pub(crate) async fn create_h3_endpoint(
        &self,
        config: Option<&ServerIdentity>,
    ) -> DnsH3Endpoint {
        let identity = config.and_then(Self::load_identity);
        self.create_h3_endpoint_from_optional_identity(identity)
            .await
    }

    pub(crate) async fn create_h3_endpoint_from_identity(
        &self,
        identity: &Identity,
    ) -> DnsH3Endpoint {
        self.create_h3_endpoint_from_optional_identity(Some(identity.clone()))
            .await
    }

    async fn create_h3_endpoint_from_optional_identity(
        &self,
        identity: Option<Identity>,
    ) -> DnsH3Endpoint {
        Endpoint::builder()
            .maybe_identity(identity.map(Arc::new))
            .dns(DnsScheme::System)
            .build()
            .await
            .expect("dns h3 client endpoint identity should be valid")
            .as_h3()
    }

    fn load_identity(config: &ServerIdentity) -> Option<Identity> {
        let (cert_path, key_path, name) =
            (&config.cert_path, &config.key_path, &config.server_name);
        let (Ok(cert_data), Ok(key_data)) = (std::fs::read(cert_path), std::fs::read(key_path))
        else {
            return None;
        };

        use std::io::Cursor;

        use rustls_pemfile::{certs, private_key};

        let cert_chain: Vec<_> = certs(&mut Cursor::new(&cert_data))
            .collect::<Result<Vec<_>, _>>()
            .expect("failed to parse certificates");

        let private_key = private_key(&mut Cursor::new(&key_data))
            .expect("failed to parse private key")
            .expect("no private key found");

        Some(Identity::new(name.clone().into(), cert_chain, private_key))
    }
}

pub async fn build_query_resolvers(node: &ConfigNode, server_name_key: &str) -> Resolvers {
    let resolver = DnsResolver::from_node_or_default(node);
    let identity = optional_server_identity(node, server_name_key);
    let endpoint = resolver.create_h3_endpoint(identity.as_ref()).await;

    Resolvers::builder()
        .h3_with_base_url(resolver.base_url.to_string(), endpoint)
        .expect("h3 dns server base_url has been checked")
        .system()
        .build()
}

pub async fn build_query_resolver_chain(servers: &[Arc<ConfigNode>]) -> Resolvers {
    let mut builder = Resolvers::builder();
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
                let endpoint = resolver.create_h3_endpoint(Some(&identity)).await;
                builder = builder
                    .h3_with_base_url(resolver.base_url.to_string(), endpoint)
                    .expect("h3 dns server base_url has been checked");
            }
        }
    }

    if seen.is_empty() {
        let resolver = DnsResolver::default_h3();
        let endpoint = resolver.create_h3_endpoint(None).await;
        builder = builder
            .h3_with_base_url(resolver.base_url.to_string(), endpoint)
            .expect("h3 dns server base_url has been checked");
    }

    builder.system().build()
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

    #[test]
    fn query_resolver_chain_uses_resolvers_builder() {
        let source = include_str!("resolve.rs");
        let h3_resolver_import = ["h3::", "H3Resolver"].concat();
        let direct_h3_resolver = ["Arc::new(\n            ", "H3Resolver::new"].concat();
        let quic_client_builder = ["Quic", "Client::builder()"].concat();
        let root_certificates = ["with_root_", "certificates"].concat();
        let alpns = ["with_", "alpns"].concat();
        let local_builder_wrapper = ["add_h3_", "query_resolver"].concat();

        assert!(source.contains("Resolvers::builder()"));
        assert!(source.contains(".h3_with_base_url("));
        assert!(source.contains(".system()"));
        assert!(!source.contains(&local_builder_wrapper));
        assert!(!source.contains("Resolvers::default()\n        .with"));
        assert!(!source.contains(&h3_resolver_import));
        assert!(!source.contains(&direct_h3_resolver));
        assert!(!source.contains(&quic_client_builder));
        assert!(!source.contains(&root_certificates));
        assert!(!source.contains(&alpns));
    }
}
