use std::{net::SocketAddr, path::PathBuf, str::FromStr};

use dhttp::name::DhttpName;
use h3x::dquic::binds::BindPattern;
use snafu::whatever;

use super::{Result, Value};

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

pub fn server_id_or_default(node: &super::Node) -> u8 {
    match node.get("server_id") {
        Some(Value::ServerId(id)) => *id,
        _ => 0,
    }
}

pub fn server_identity(
    node: &super::Node,
    server_name: DhttpName<'static>,
) -> Option<ServerIdentity> {
    let (Some(Value::Path(cert_path)), Some(Value::Path(key_path))) =
        (node.get("ssl_certificate"), node.get("ssl_certificate_key"))
    else {
        return None;
    };

    Some(ServerIdentity {
        cert_path: cert_path.clone(),
        key_path: key_path.clone(),
        server_name,
        server_id: server_id_or_default(node),
    })
}

pub fn optional_server_identity(
    node: &super::Node,
    server_name_key: &str,
) -> Option<ServerIdentity> {
    let Some(Value::String(server_name)) = node.get(server_name_key) else {
        return None;
    };

    let server_name = DhttpName::try_from(server_name.clone()).ok()?;
    server_identity(node, server_name)
}

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
            IfaceRange::External | IfaceRange::Internal => {
                tracing::warn!(
                    "iface range external/internal is not implemented yet, treating as non-match"
                );
                false
            }
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

impl Listens {
    pub fn new(range: IfaceRange, families: IpFamilies, port: u16) -> Self {
        Self {
            range,
            families,
            port,
            specific_addrs: None,
        }
    }

    pub fn to_bind_patterns(&self) -> Vec<BindPattern> {
        fn parse_pattern(input: String) -> BindPattern {
            input
                .parse()
                .expect("generated bind pattern should be valid")
        }

        if let Some(specific_addrs) = &self.specific_addrs {
            return specific_addrs
                .iter()
                .map(|addr| parse_pattern(format!("inet://{addr}")))
                .collect();
        }

        let host = match &self.range {
            IfaceRange::All => "*",
            IfaceRange::Exact(name) => name.as_str(),
            IfaceRange::External | IfaceRange::Internal => {
                // External/Internal require classifying interfaces via
                // routing-table and loopback checks. Keep them explicitly
                // unimplemented instead of silently changing the bind set.
                unimplemented!("iface range external/internal is not implemented yet")
            }
        };

        let family_prefix = match self.families {
            IpFamilies::V4 => "v4.",
            IpFamilies::V6 => "v6.",
            IpFamilies::Dual => "",
        };

        vec![parse_pattern(format!(
            "iface://{family_prefix}{host}:{}",
            self.port
        ))]
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

    use super::*;

    fn pattern_strings(listen: Listens) -> Vec<String> {
        listen
            .to_bind_patterns()
            .into_iter()
            .map(|pattern| pattern.to_string())
            .collect()
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
}
