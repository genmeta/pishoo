use std::{net::SocketAddr, path::PathBuf, str::FromStr};

use dquic::prelude::BindUri;
use snafu::whatever;

use super::{Result, Value};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServerName {
    pub name: String,
}

// TLS identity configuration for clients/servers that need certificates
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerIdentity {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    pub server_name: String,
    pub server_id: u8,
}

pub fn server_id_or_default(node: &super::Node) -> u8 {
    match node.get("server_id") {
        Some(Value::ServerId(id)) => *id,
        _ => 0,
    }
}

pub fn server_identity(node: &super::Node, server_name: String) -> Option<ServerIdentity> {
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

    server_identity(node, server_name.clone())
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

    pub fn contains(&self, bind_uri: &BindUri) -> bool {
        use dquic::qbase::net::*;
        // TODO: check specific_addrs
        let Some((ip_family, device, port)) = bind_uri.as_iface_bind_uri() else {
            return false;
        };

        (matches!(self.families, IpFamilies::Dual)
            || (matches!(self.families, IpFamilies::V4) && matches!(ip_family, Family::V4))
            || (matches!(self.families, IpFamilies::V6) && matches!(ip_family, Family::V6)))
            && self.range.contains(device)
            && self.port == port
    }

    pub fn resolve<'i, I>(&'i self, devices: I) -> Box<dyn Iterator<Item = BindUri> + Send + 'i>
    where
        I: IntoIterator<Item = &'i str>,
        I::IntoIter: Send + 'i,
    {
        if let Some(ref specific_addrs) = self.specific_addrs {
            return Box::new(specific_addrs.clone().into_iter().map(BindUri::from));
        }

        Box::new(
            devices
                .into_iter()
                .filter(|name| {
                    matches!(self.range, IfaceRange::All)
                        || matches!(self.range, IfaceRange::Exact(ref iface_name) if iface_name == *name)
                })
                .flat_map(move |name| {
                    let mut ipv4_bind_uri =
                        BindUri::from(format!("iface://v4.{name}:{}", self.port));
                    let mut ipv6_bind_uri =
                        BindUri::from(format!("iface://v6.{name}:{}", self.port));
                    if self.port == 0 {
                        ipv4_bind_uri = ipv4_bind_uri.alloc_port();
                        ipv6_bind_uri = ipv6_bind_uri.alloc_port();
                    }

                    match self.families {
                        IpFamilies::V4 => [Some(ipv4_bind_uri), None],
                        IpFamilies::V6 => [None, Some(ipv6_bind_uri)],
                        IpFamilies::Dual => [Some(ipv4_bind_uri), Some(ipv6_bind_uri)],
                    }
                    .into_iter()
                    .flatten()
                }),
        )
    }
}
