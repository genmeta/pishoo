use std::{fmt, net::SocketAddr, path::PathBuf, str::FromStr};

use dhttp::name::DhttpName;
use h3x::dquic::binds::BindPattern;
use snafu::{Snafu, whatever};

use super::Result;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServerName {
    pub name: DhttpName<'static>,
}

// TLS identity configuration for clients/servers that need certificates
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerIdentity {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    pub server_name: DhttpName<'static>,
    pub server_id: u8,
}

pub fn server_id_or_default(node: &crate::parse::document::ConfigNode) -> u8 {
    node.get::<ServerIdConfig>("server_id")
        .ok()
        .flatten()
        .map(|id| id.0)
        .unwrap_or(0)
}

pub fn server_identity(
    node: &crate::parse::document::ConfigNode,
    server_name: DhttpName<'static>,
) -> Option<ServerIdentity> {
    let cert_path = node.get::<PathConfig>("ssl_certificate").ok().flatten()?;
    let key_path = node
        .get::<PathConfig>("ssl_certificate_key")
        .ok()
        .flatten()?;
    Some(ServerIdentity {
        cert_path: cert_path.0.clone(),
        key_path: key_path.0.clone(),
        server_name,
        server_id: server_id_or_default(node),
    })
}

pub fn optional_server_identity(
    node: &crate::parse::document::ConfigNode,
    server_name_key: &str,
) -> Option<ServerIdentity> {
    let server_name = node
        .get::<ClientNameConfig>(server_name_key)
        .ok()
        .flatten()?;
    server_identity(node, server_name.0.clone())
}

#[derive(Debug, Clone)]
pub struct BoolConfig(pub bool);

#[derive(Debug, Clone)]
pub struct StringConfig(pub String);

#[derive(Debug, Clone)]
pub struct StringList(pub Vec<String>);

#[derive(Debug, Clone)]
pub struct PathConfig(pub std::path::PathBuf);

#[derive(Debug, Clone)]
pub struct AccessRulesUri(pub url::Url);

#[derive(Debug, Clone)]
pub struct ProxyPass(pub http::Uri);

#[derive(Debug, Clone)]
pub struct ResolverConfig(pub http::Uri);

#[derive(Debug, Clone)]
pub struct SocketAddrs(pub Vec<std::net::SocketAddr>);

#[derive(Debug, Clone)]
pub struct ListenConfig(pub Vec<Listens>);

#[derive(Debug, Clone)]
pub struct ServerNames(pub Vec<ServerName>);

#[derive(Debug, Clone)]
pub struct ClientNameConfig(pub DhttpName<'static>);

#[derive(Debug, Clone)]
pub struct ServerIdConfig(pub u8);

#[derive(Debug, Clone)]
pub struct HeaderRule {
    pub name: http::HeaderName,
    pub value: http::HeaderValue,
    pub always: bool,
}

#[derive(Debug, Clone)]
pub struct HeaderRules(pub Vec<HeaderRule>);

#[derive(Debug, Clone)]
pub struct MimeTypes(pub std::collections::HashMap<String, http::HeaderValue>);

#[derive(Debug, Clone)]
pub struct DefaultType(pub http::HeaderValue);

#[derive(Debug, Clone)]
pub struct GzipMinLength(pub u64);

#[derive(Debug, Clone)]
pub struct GzipCompLevel(pub i32);

#[derive(Debug, Clone)]
pub struct SshLoginMethods(pub Vec<String>);

#[derive(Debug, Clone)]
pub struct SshSslUser {
    pub name: String,
    pub user: String,
}

#[derive(Debug, Clone)]
pub struct SshSslUsers(pub Vec<SshSslUser>);

#[derive(Debug, Clone)]
pub struct StunBindConfigValue {
    pub bind: std::net::SocketAddr,
    pub outer_addr: Option<std::net::SocketAddr>,
    pub change_addr: Option<std::net::SocketAddr>,
    pub change_port: Option<u16>,
}

#[derive(Debug, Clone)]
pub struct StunChangePort(pub u16);

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum IpFamilies {
    V4,
    V6,
    #[default]
    Dual,
}

impl FromStr for IpFamilies {
    type Err = crate::error::Whatever;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "v4only" => Ok(IpFamilies::V4),
            "v6only" => Ok(IpFamilies::V6),
            "dual" => Ok(IpFamilies::Dual),
            _ => whatever!("invalid ip families: {s}, expected `v4only`, `v6only` or `dual`"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum IfaceRange {
    All,
    External,
    Internal,
    Exact(String),
}

impl IfaceRange {
    pub fn contains(&self, iface_name: &str) -> bool {
        match self {
            IfaceRange::All => true,
            IfaceRange::Exact(name) => name == iface_name,
            IfaceRange::Internal => matches!(iface_name, "lo" | "lo0"),
            IfaceRange::External => {
                tracing::warn!(
                    "iface range external is not implemented yet, treating as non-match"
                );
                false
            }
        }
    }
}

impl fmt::Display for IfaceRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::All => f.write_str("all"),
            Self::External => f.write_str("external"),
            Self::Internal => f.write_str("internal"),
            Self::Exact(name) => f.write_str(name),
        }
    }
}

impl From<&str> for IfaceRange {
    fn from(value: &str) -> Self {
        match value {
            "all" => IfaceRange::All,
            "external" => IfaceRange::External,
            "internal" => IfaceRange::Internal,
            _ => IfaceRange::Exact(value.to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Listens {
    pub range: IfaceRange,
    pub families: IpFamilies,
    pub port: u16,
    pub specific_addrs: Option<Vec<SocketAddr>>,
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum ListenBindPatternError {
    #[snafu(display("unsupported listen iface range `{range}`"))]
    UnsupportedIfaceRange { range: IfaceRange },
}

impl Listens {
    pub fn new(range: IfaceRange, families: IpFamilies, port: u16) -> Self {
        Self {
            range,
            families,
            port,
            specific_addrs: None,
        }
    }

    pub fn try_to_bind_patterns(&self) -> Result<Vec<BindPattern>, ListenBindPatternError> {
        fn parse_pattern(input: String) -> BindPattern {
            input
                .parse()
                .expect("generated bind pattern should be valid")
        }

        if let Some(specific_addrs) = &self.specific_addrs {
            return Ok(specific_addrs
                .iter()
                .map(|addr| parse_pattern(format!("inet://{addr}")))
                .collect());
        }

        let host = match &self.range {
            IfaceRange::All => "*",
            IfaceRange::Exact(name) => name.as_str(),
            IfaceRange::Internal => {
                return Ok(match self.families {
                    IpFamilies::V4 => {
                        vec![parse_pattern(format!("inet://127.0.0.1:{}", self.port))]
                    }
                    IpFamilies::V6 => vec![parse_pattern(format!("inet://[::1]:{}", self.port))],
                    IpFamilies::Dual => vec![
                        parse_pattern(format!("inet://127.0.0.1:{}", self.port)),
                        parse_pattern(format!("inet://[::1]:{}", self.port)),
                    ],
                });
            }
            IfaceRange::External => {
                return listen_bind_pattern_error::UnsupportedIfaceRangeSnafu {
                    range: self.range.clone(),
                }
                .fail();
            }
        };

        let family_prefix = match self.families {
            IpFamilies::V4 => "v4.",
            IpFamilies::V6 => "v6.",
            IpFamilies::Dual => "",
        };

        Ok(vec![parse_pattern(format!(
            "iface://{family_prefix}{host}:{}",
            self.port
        ))])
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

    use super::*;

    fn pattern_strings(listen: Listens) -> Vec<String> {
        listen
            .try_to_bind_patterns()
            .expect("listen should produce bind patterns")
            .into_iter()
            .map(|pattern| pattern.to_string())
            .collect()
    }

    fn try_pattern_strings(listen: Listens) -> Result<Vec<String>, ListenBindPatternError> {
        listen.try_to_bind_patterns().map(|patterns| {
            patterns
                .into_iter()
                .map(|pattern| pattern.to_string())
                .collect()
        })
    }

    #[test]
    fn listens_all_dual_preserves_wildcard_pattern() {
        let listen = Listens::new(IfaceRange::All, IpFamilies::Dual, 443);

        assert_eq!(pattern_strings(listen), vec!["iface://*:443"]);
    }

    #[test]
    fn listens_exact_family_preserves_family_pattern() {
        assert_eq!(
            pattern_strings(Listens::new(
                IfaceRange::Exact("eth0".to_owned()),
                IpFamilies::V4,
                443
            )),
            vec!["iface://v4.eth0:443"]
        );
        assert_eq!(
            pattern_strings(Listens::new(
                IfaceRange::Exact("eth0".to_owned()),
                IpFamilies::V6,
                443
            )),
            vec!["iface://v6.eth0:443"]
        );
    }

    #[test]
    fn listens_specific_addrs_become_inet_patterns() {
        let mut listen = Listens::new(IfaceRange::All, IpFamilies::Dual, 443);
        listen.specific_addrs = Some(vec![
            SocketAddr::from((Ipv4Addr::LOCALHOST, 8443)),
            SocketAddr::from((Ipv6Addr::LOCALHOST, 9443)),
        ]);

        assert_eq!(
            pattern_strings(listen),
            vec!["inet://127.0.0.1:8443", "inet://[::1]:9443"]
        );
    }

    #[test]
    fn listens_internal_dual_becomes_loopback_patterns() {
        assert_eq!(
            try_pattern_strings(Listens::new(IfaceRange::Internal, IpFamilies::Dual, 443))
                .expect("internal dual listen should be supported"),
            vec!["inet://127.0.0.1:443", "inet://[::1]:443"]
        );
    }

    #[test]
    fn listens_internal_family_becomes_matching_loopback_pattern() {
        assert_eq!(
            try_pattern_strings(Listens::new(IfaceRange::Internal, IpFamilies::V4, 443))
                .expect("internal v4 listen should be supported"),
            vec!["inet://127.0.0.1:443"]
        );
        assert_eq!(
            try_pattern_strings(Listens::new(IfaceRange::Internal, IpFamilies::V6, 443))
                .expect("internal v6 listen should be supported"),
            vec!["inet://[::1]:443"]
        );
    }

    #[test]
    fn listens_external_returns_typed_error() {
        let error = Listens::new(IfaceRange::External, IpFamilies::Dual, 443)
            .try_to_bind_patterns()
            .expect_err("external listen should be explicitly unsupported");

        assert!(matches!(
            error,
            ListenBindPatternError::UnsupportedIfaceRange { .. }
        ));
    }
}
