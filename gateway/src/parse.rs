use std::{
    collections::HashMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Weak},
};

use anyhow::{Result, anyhow};
use conf::parse_conf;
use http::{HeaderName, HeaderValue, Uri};
use misc_conf::{
    ast::{Directive, DirectiveTrait},
    nginx::Nginx,
};
use pattern::Pattern;
use qdns::Resolve;
use tokio::sync::OnceCell;
use tracing::error;

pub mod conf;
mod location;
mod pattern;
mod pishoo;
mod proxy;
mod server;

type ParseFn = Box<dyn Fn(Directive<Nginx>) -> Result<Value>>;

#[derive(Debug)]
pub enum Value {
    String(String),
    Resolver(Resolver),
    StringVec(Vec<String>),
    StringMap(HashMap<String, String>),
    Boolean(bool),
    Addr(SocketAddr),
    Path(PathBuf),
    Header(Vec<(HeaderName, HeaderValue, bool)>),
    Types(HashMap<String, HeaderValue>),
    HeaderValue(HeaderValue),
    Listen(Vec<Listen>),
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

#[derive(Debug, PartialEq, Eq, Hash)]
pub enum Resolver {
    Udp { server_addr: SocketAddr },
    Http { base_url: Uri },
}

impl From<&Resolver> for Arc<dyn Resolve + Send + Sync> {
    fn from(resolver: &Resolver) -> Self {
        use qdns::*;
        match resolver {
            Resolver::Udp { server_addr } => {
                Arc::new(UdpResolver::new(*server_addr)) as Arc<dyn Resolve + Send + Sync>
            }
            Resolver::Http { base_url } => Arc::new(
                HttpResolver::new(base_url.to_string())
                    .expect("HTTP dns server base_url has been checked"),
            ),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub enum IPVersion {
    V4,
    V6,
    #[default]
    Dual,
}

impl TryFrom<&str> for IPVersion {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self> {
        match value {
            "v4only" => Ok(IPVersion::V4),
            "v6only" => Ok(IPVersion::V6),
            "dual" => Ok(IPVersion::Dual),
            _ => Err(anyhow!("Invalid IP version: {}", value)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum IfaceType {
    All,
    External,
    Internal,
    Exact(String),
}

impl From<&str> for IfaceType {
    fn from(value: &str) -> Self {
        match value {
            "all" => IfaceType::All,
            "external" => IfaceType::External,
            "internal" => IfaceType::Internal,
            _ => IfaceType::Exact(value.to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Listen {
    pub typ: IfaceType,
    pub version: IPVersion,
    pub port: u16,
}

// 包含数据和父链接的节点结构
#[derive(Debug)]
pub struct Node {
    // 使用RwLock实现内部可变性
    value: Value,
    // Weak指针避免循环引用
    parent: OnceCell<Option<Weak<Node>>>,
}

impl Node {
    pub fn new(value: Value) -> Self {
        assert!(matches!(value, Value::ValueMap(..) | Value::Pattern(..)));

        Self {
            value,
            parent: OnceCell::new(),
        }
    }

    // 获取存活的父节点Arc
    pub fn parent(&self) -> Option<Arc<Node>> {
        self.parent
            .get()
            .and_then(|opt_weak_ref| opt_weak_ref.as_ref())
            .and_then(|weak_parent| weak_parent.upgrade())
    }

    // 不可变访问节点值
    pub fn value(&self) -> &Value {
        &self.value
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.value.get(key)
    }

    fn set_parent(&self, parent: Option<Weak<Node>>) {
        self.parent.set(parent).expect("Parent link set multiple times for the same node. This indicates a bug in the tree transformation logic.");
    }

    pub fn backtrack_node(self: &Arc<Self>, key: &str) -> Option<Arc<Node>> {
        let mut current_node = Arc::clone(self);
        loop {
            if current_node.value().get(key).is_some() {
                return Some(Arc::clone(&current_node));
            }
            let parent = current_node.parent()?;
            current_node = parent;
        }
    }
}

pub fn parse(configure: &[u8], root: Option<&Path>) -> Result<Arc<Node>> {
    let mut directives = Directive::<Nginx>::parse(configure)?;

    if let Some(root) = root {
        directives = directives
            .into_iter()
            .map(|mut directive| directive.resolve_include(root).map(|_| directive))
            .collect::<Result<Vec<_>>>()?;
    }

    parse_conf(directives).inspect_err(|e| error!("Error parsing directives: {}", e))
}

#[allow(dead_code)]
fn parse_string_map(directive: Directive<Nginx>) -> Result<Value> {
    if let Some(children) = directive.children {
        let mut map = HashMap::new();
        for directive in children {
            let value = directive.name;
            for arg in directive.args {
                map.insert(arg, value.clone());
            }
        }
        return Ok(Value::StringMap(map));
    }
    Ok(Value::ValueMap(HashMap::new()))
}

fn parse_header_value(directive: Directive<Nginx>) -> Result<Value> {
    match &directive.args[..] {
        [value] => {
            let header_value = HeaderValue::from_str(value)?;
            Ok(Value::HeaderValue(header_value))
        }
        _ => Err(anyhow!(
            "Invalid number of arguments for directive: {}",
            directive.name
        )),
    }
}

fn parse_types(directive: Directive<Nginx>) -> Result<Value> {
    if let Some(children) = directive.children {
        let mut map = HashMap::new();
        for directive in children {
            let value = HeaderValue::from_str(directive.name.as_str())?;
            for arg in directive.args {
                map.insert(arg, value.clone());
            }
        }
        return Ok(Value::Types(map));
    }
    Ok(Value::ValueMap(HashMap::new()))
}

fn parse_string(directive: Directive<Nginx>) -> Result<Value> {
    match &directive.args[..] {
        [string] => Ok(Value::String(string.to_string())),
        _ => Err(anyhow!(
            "Invalid number of arguments for directive: {}",
            directive.name
        )),
    }
}

/// listen [all/external/internal/lo/en0] [v6only|v4only|dual] [0|80]
fn parse_listen(directive: Directive<Nginx>) -> Result<Value> {
    match &directive.args[..] {
        [iface] => {
            // 单个参数 只能是网卡名, 省略了 version 和端口的情况
            Ok(Value::Listen(vec![Listen {
                typ: IfaceType::from(iface.as_str()),
                version: IPVersion::default(),
                port: 0,
            }]))
        }
        [iface, param] => {
            // 两个参数, 可能是
            // 1. 网卡名和 v6only|v4only|dual
            // 2. 网卡名和端口

            let typ = IfaceType::from(iface.as_str());
            let version = IPVersion::try_from(param.as_str()).ok();
            let port = if version.is_none() {
                param.parse::<u16>().ok()
            } else {
                None
            };
            if version.is_none() && port.is_none() {
                return Err(anyhow!(
                    "Invalid argument for directive: {}:{}",
                    directive.name,
                    param
                ));
            }

            Ok(Value::Listen(vec![Listen {
                typ,
                version: version.unwrap_or_default(),
                port: port.unwrap_or_default(),
            }]))
        }
        [iface, version, port] => {
            // 三个参数, 只能是 网卡名和 v6only|v4only|dual 和端口
            let typ = IfaceType::from(iface.as_str());
            let version = IPVersion::try_from(version.as_str())?;
            let port = port.parse::<u16>().map_err(|e| {
                anyhow!(
                    "Invalid port while parsing listen directive: {}:{}",
                    directive.name,
                    e
                )
            })?;
            Ok(Value::Listen(vec![Listen { typ, version, port }]))
        }
        _ => Err(anyhow!(
            "Invalid number of arguments for directive: {}",
            directive.name
        )),
    }
}

fn parse_address(directive: Directive<Nginx>) -> Result<Value> {
    match &directive.args[..] {
        [string] => {
            let addr = string.parse::<SocketAddr>()?;
            Ok(Value::Addr(addr))
        }
        _ => Err(anyhow!(
            "Invalid number of arguments for directive: {}",
            directive.name
        )),
    }
}

fn parse_resolver(directive: Directive<Nginx>) -> Result<Value> {
    match &directive.args[..] {
        [kind, resolver] => match kind.as_str() {
            "udp" => {
                let server_addr = resolver
                    .parse::<SocketAddr>()
                    .map_err(|e| anyhow!("Invalid address whiling parsing udp resolver: {e:?}",))?;
                Ok(Value::Resolver(Resolver::Udp { server_addr }))
            }
            "http" => {
                let base_url = resolver.parse::<Uri>().map_err(|e| {
                    anyhow!("Invalid address whiling parsing http resolver: {e:?}",)
                })?;
                Ok(Value::Resolver(Resolver::Http { base_url }))
            }
            _ => Err(anyhow!(
                "Unknown resolver kind: {kind}, expected `udp` or `http`"
            )),
        },
        _ => Err(anyhow!(
            "Invalid number of arguments for directive: {}",
            directive.name
        )),
    }
}

fn parse_path(directive: Directive<Nginx>) -> Result<Value> {
    match &directive.args[..] {
        [string] => {
            let path = PathBuf::from(string);
            if !path.exists() {
                return Err(anyhow!("Path does not exist: {}", string));
            }
            Ok(Value::Path(path))
        }
        _ => Err(anyhow!(
            "Invalid number of arguments for directive: {}",
            directive.name
        )),
    }
}

fn parse_string_vec(directive: Directive<Nginx>) -> Result<Value> {
    Ok(Value::StringVec(directive.args))
}

fn parse_header(directive: Directive<Nginx>) -> Result<Value> {
    match &directive.args[..] {
        [name, value] => {
            let header_name = HeaderName::from_bytes(name.as_bytes())?;
            let header_value = HeaderValue::from_bytes(value.as_bytes())?;
            Ok(Value::Header(vec![(header_name, header_value, true)]))
        }
        _ => Err(anyhow!(
            "Invalid number of arguments for directive: {}",
            directive.name
        )),
    }
}

fn parse_header_always(directive: Directive<Nginx>) -> Result<Value> {
    match &directive.args[..] {
        [name, value] => {
            let header_name = HeaderName::from_bytes(name.as_bytes())?;
            let header_value = HeaderValue::from_bytes(value.as_bytes())?;
            Ok(Value::Header(vec![(header_name, header_value, false)]))
        }
        [name, value, always] => {
            if always.as_str() != "always" {
                return Err(anyhow!("Invalid argument for always: {}", always));
            }
            let header_name = HeaderName::from_bytes(name.as_bytes())?;
            let header_value = HeaderValue::from_bytes(value.as_bytes())?;
            Ok(Value::Header(vec![(header_name, header_value, true)]))
        }
        _ => Err(anyhow!(
            "Invalid number of arguments for directive: {}",
            directive.name
        )),
    }
}

fn parse_ssh_login(directive: Directive<Nginx>) -> Result<Value> {
    match &directive.args[..] {
        [auth] => {
            if auth != "basic" && auth != "ssl" {
                Err(anyhow!("Invalid value for directive: {}", directive.name))
            } else {
                Ok(Value::String(auth.to_string()))
            }
        }
        _ => Err(anyhow!("Invalid value for directive: {}", directive.name)),
    }
}

fn parse_ssh_ssl_user(directive: Directive<Nginx>) -> Result<Value> {
    match &directive.args[..] {
        [name, user] => Ok(Value::SshSslUser(vec![(
            name.to_string(),
            user.to_string(),
        )])),
        _ => Err(anyhow!(
            "Invalid number of arguments for directive: {}",
            directive.name
        )),
    }
}
