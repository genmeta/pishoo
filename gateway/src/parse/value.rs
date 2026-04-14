use std::{collections::HashMap, path::PathBuf, sync::Arc};

use http::{HeaderName, HeaderValue, Uri};

use super::{Node, ServerName, pattern::Pattern, types::Listens};

#[derive(Debug, Clone)]
pub enum Value {
    String(String),
    Uri(Uri),
    Resolver(Uri),
    StringVec(Vec<String>),
    ServerName(Vec<ServerName>),
    ServerId(u8),
    StringMap(HashMap<String, String>),
    Boolean(bool),
    Addr(std::net::SocketAddr),
    AddrVec(Vec<std::net::SocketAddr>),
    Path(PathBuf),
    Header(Vec<(HeaderName, HeaderValue, bool)>),
    Types(HashMap<String, HeaderValue>),
    HeaderValue(HeaderValue),
    Listen(Vec<Listens>),
    Pattern(Pattern, HashMap<String, Value>),
    SshSslUser(Vec<(String, String)>),
    ValueMap(HashMap<String, Value>),
    Nodes(Vec<Arc<Node>>),
}

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::ValueMap(map) => map.get(key),
            Value::Pattern(_, map) => map.get(key),
            _ => None,
        }
    }
}
