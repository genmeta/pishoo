pub mod publish;
pub mod resolve;

pub const HTTP_DNS_SERVER: &str = "https://dns.genmeta.net/";
pub const H3_DNS_SERVER: &str = "https://dns.genmeta.net:4433";
pub const DEFAULT_STUN_SERVER: &str = "nat.genmeta.net:20004";
pub const MDNS_SERVICE: &str = "_genmeta.local";

pub use publish::{
    BindUriProvider, PublishConfig, build_publish_config_from_identity, build_publish_configs,
    publish_host_endpoints, publish_server, spawn_server_publish_task,
};
pub use resolve::{DnsResolver, build_query_resolver_chain, build_query_resolvers};
