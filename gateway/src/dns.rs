pub mod publish;
pub mod resolve;

pub const HTTP_DNS_SERVER: &str = "https://dns.genmeta.net/";
pub const H3_DNS_SERVER: &str = "https://dns.genmeta.net:4433";
pub const MDNS_SERVICE: &str = "_genmeta.local";

pub use publish::{
    PublishConfig, Publisher, build_publish_configs, publish_host_endpoints, publish_now,
};
pub use resolve::{DnsResolver, build_query_resolver_chain, build_query_resolvers};
