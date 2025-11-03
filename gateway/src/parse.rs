use std::{
    collections::HashMap,
    fmt::Display,
    net::SocketAddr,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, Weak},
};

use conf::parse_conf;
use gm_quic::prelude::BindUri;
use http::{HeaderName, HeaderValue, Uri};
use misc_conf::{
    ast::{Directive, DirectiveTrait},
    nginx::Nginx,
};
use pattern::Pattern;
use qdns::Resolve;
use snafu::{OptionExt, ResultExt, ensure_whatever, whatever};
use tokio::sync::OnceCell;

use crate::error::Whatever;

pub mod conf;
mod location;
pub mod pattern;
mod pishoo;
mod proxy;
mod server;

type Result<T, E = Whatever> = std::result::Result<T, E>;

type ParseFn = fn(Directive<Nginx>) -> Result<Value>;

#[derive(Debug, Clone)]
pub enum Value {
    String(String),
    Uri(Uri),
    Resolver(Resolver),
    StringVec(Vec<String>),
    StringMap(HashMap<String, String>),
    Boolean(bool),
    Addr(SocketAddr),
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Resolver {
    Http { base_url: Uri },
}

impl Display for Resolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Resolver::Http { base_url } => write!(f, "http {} ", base_url),
        }
    }
}

impl From<&Resolver> for Arc<dyn Resolve + Send + Sync> {
    fn from(resolver: &Resolver) -> Self {
        use qdns::*;
        match resolver {
            Resolver::Http { base_url } => Arc::new(
                HttpResolver::new(base_url.to_string())
                    .expect("HTTP dns server base_url has been checked"),
            ),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub enum IpFamilies {
    V4,
    V6,
    #[default]
    Dual,
}

impl FromStr for IpFamilies {
    type Err = Whatever;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "v4only" => Ok(IpFamilies::V4),
            "v6only" => Ok(IpFamilies::V6),
            "dual" => Ok(IpFamilies::Dual),
            _ => whatever!("Invalid IP families: {s}, expected `v4only`, `v6only` or `dual`"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
            IfaceRange::External | IfaceRange::Internal => unimplemented!(),
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Listens {
    pub range: IfaceRange,
    pub families: IpFamilies,
    pub port: u16,
}

impl Listens {
    pub fn new(range: IfaceRange, families: IpFamilies, port: u16) -> Self {
        Self {
            range,
            families,
            port,
        }
    }

    pub fn contains(&self, bind_uri: &BindUri) -> bool {
        use gm_quic::qbase::net::*;
        let Some((ip_family, device, port)) = bind_uri.as_iface_bind_uri() else {
            return false;
        };

        (matches!(self.families, IpFamilies::Dual)
            || (matches!(self.families, IpFamilies::V4) && matches!(ip_family, Family::V4))
            || (matches!(self.families, IpFamilies::V6) && matches!(ip_family, Family::V6)))
            && self.range.contains(device)
            && self.port == port
    }

    pub fn resolve<'i>(
        &self,
        devices: impl IntoIterator<Item = &'i str>,
    ) -> impl Iterator<Item = BindUri> {
        devices.into_iter().filter(|name| {
            (matches!(self.range, IfaceRange::All)
                || matches!(self.range, IfaceRange::Exact(ref iface_name) if iface_name == name))
        })
        .flat_map(move |name| {
            let mut ipv4_bind_uri = BindUri::from( format!("iface://v4.{name}:{}",self.port));
            let mut ipv6_bind_uri = BindUri::from( format!("iface://v6.{name}:{}",self.port));
            if self.port == 0 {
                ipv4_bind_uri = ipv4_bind_uri.alloc_port();
                ipv6_bind_uri = ipv6_bind_uri.alloc_port();
            }

            match self.families {
                IpFamilies::V4 => [Some(ipv4_bind_uri),None],
                IpFamilies::V6 => [None,Some(ipv6_bind_uri)],
                IpFamilies::Dual => [Some(ipv4_bind_uri),Some(ipv6_bind_uri)],
            }.into_iter().flatten()
        })
    }
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
    let mut directives =
        Directive::<Nginx>::parse(configure).whatever_context("Cannot parse configuration")?;

    // 预处理
    if let Some(root) = root {
        directives = directives
            .into_iter()
            .map(|mut directive| directive.resolve_include(root).map(|_| directive))
            .collect::<Result<Vec<_>, _>>()
            .whatever_context("Cannot resolve include in configuration")?;
    } else {
        tracing::warn!(target:"config", "Config file has no parent, unable to resolve includes");
    }

    // 解析配置
    parse_conf(directives)
}

#[derive(Default, Debug, Clone)]
struct Commands(HashMap<&'static str, ParseFn>);

impl Commands {
    fn new() -> Self {
        Self(HashMap::new())
    }

    fn insert(&mut self, name: &'static str, command: ParseFn) {
        self.0.insert(name, command);
    }

    fn parse(
        &self,
        directives: impl IntoIterator<Item = Directive<Nginx>>,
    ) -> Result<HashMap<String, Value>> {
        let mut values = HashMap::new();
        for directive in directives {
            let name = directive.name.clone();
            let Some(command) = self.0.get(name.as_str()) else {
                whatever!("Unknown directive `{name}`",);
            };

            match command(directive)? {
                value @ (Value::ValueMap(..) | Value::Pattern(..)) => {
                    let Value::Nodes(nodes) =
                        values.entry(name).or_insert_with(|| Value::Nodes(vec![]))
                    else {
                        unreachable!("Unexpected value type, should be `Nodes`");
                    };
                    nodes.push(Arc::new(Node::new(value)));
                }
                Value::Header(headers) => {
                    let Value::Header(exist_headers) =
                        values.entry(name).or_insert_with(|| Value::Header(vec![]))
                    else {
                        unreachable!("Unexpected value type, should be `Header`");
                    };
                    exist_headers.extend(headers);
                }
                Value::SshSslUser(users) => {
                    let Value::SshSslUser(exist_users) =
                        values.entry(name).or_insert_with(|| Value::Header(vec![]))
                    else {
                        unreachable!("Unexpected value type, should be `Header`");
                    };
                    exist_users.extend(users);
                }
                value => _ = values.insert(name, value),
            }
        }
        Ok(values)
    }
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
            let header_value = HeaderValue::from_str(value)
                .whatever_context(format!("Failed to parse `{value}` to header value"))?;
            Ok(Value::HeaderValue(header_value))
        }
        _ => whatever!(
            "Invalid number of arguments for directive: {}",
            directive.name
        ),
    }
}

fn parse_types(directive: Directive<Nginx>) -> Result<Value> {
    if let Some(children) = directive.children {
        let mut map = HashMap::new();
        for directive in children {
            let value = directive.name.as_str();
            let value = HeaderValue::from_str(value)
                .whatever_context(format!("Failed to parse `{value}` to header value"))?;
            for arg in directive.args {
                map.insert(arg, value.clone());
            }
        }
        return Ok(Value::Types(map));
    }
    Ok(Value::ValueMap(HashMap::new()))
}

#[allow(dead_code)]
fn parse_string(directive: Directive<Nginx>) -> Result<Value> {
    match &directive.args[..] {
        [string] => Ok(Value::String(string.to_string())),
        _ => whatever!(
            "Invalid number of arguments for directive: {}",
            directive.name
        ),
    }
}

// fn parse_uri(directive: Directive<Nginx>) -> Result<Value> {
//     match &directive.args[..] {
//         [s] => {
//             let uri = s.parse::<Uri>().whatever_context(format!(
//                 "Invalid URI `{s}` while parsing directive {}",
//                 directive.name
//             ))?;

//             Ok(Value::Uri(uri))
//         }
//         _ => whatever!(
//             "Invalid number of arguments for directive: {}",
//             directive.name
//         ),
//     }
// }

fn parse_proxy_pass(directive: Directive<Nginx>) -> Result<Value> {
    match &directive.args[..] {
        [s] => {
            let uri = s.parse::<Uri>().whatever_context(format!(
                "Invalid URI `{s}` while parsing directive {}",
                directive.name
            ))?;

            uri.host()
                .whatever_context::<_, Whatever>("Missing host in proxy_pass URI")
                .whatever_context(format!(
                    "Invalid URI `{s}` while parsing directive {}",
                    directive.name
                ))?;

            Ok(Value::Uri(uri))
        }
        _ => whatever!(
            "Invalid number of arguments for directive: {}",
            directive.name
        ),
    }
}

/// listen [all/external/internal/lo/en0] [v6only|v4only|dual] [0|80]
fn parse_listen(directive: Directive<Nginx>) -> Result<Value> {
    match &directive.args[..] {
        [iface] => {
            // 单个参数 只能是网卡名, 省略了 families 和端口的情况
            Ok(Value::Listen(vec![Listens {
                range: IfaceRange::from(iface.as_str()),
                families: IpFamilies::default(),
                port: 0,
            }]))
        }
        [iface, param] => {
            // 两个参数, 可能是
            // 1. 网卡名和 v6only|v4only|dual
            // 2. 网卡名和端口

            let range = IfaceRange::from(iface.as_str());
            match IpFamilies::from_str(param) {
                Ok(families) => Ok(Value::Listen(vec![Listens {
                    range,
                    families,
                    port: 0,
                }])),
                Err(error) => {
                    let port = param
                        .parse::<u16>()
                        .map_err(|int_error| {
                            format!("`{param}` is neither valid IP families({error}) nor port number({int_error})")
                        })
                        .whatever_context(format!(
                            "Invalid argument for directive: {}:{}",
                            directive.name, param
                        ))?;
                    Ok(Value::Listen(vec![Listens {
                        range,
                        families: IpFamilies::default(),
                        port,
                    }]))
                }
            }
        }
        [iface, version, port] => {
            // 三个参数, 只能是 网卡名和 v6only|v4only|dual 和端口
            let range = IfaceRange::from(iface.as_str());
            let families = IpFamilies::from_str(version.as_str())?;
            let port = port.parse::<u16>().whatever_context(format!(
                "Invalid port number `{port}` while parsing directive {}",
                directive.name
            ))?;
            Ok(Value::Listen(vec![Listens {
                range,
                families,
                port,
            }]))
        }
        _ => whatever!(
            "Invalid number of arguments for directive: {}",
            directive.name
        ),
    }
}

fn parse_address(directive: Directive<Nginx>) -> Result<Value> {
    match &directive.args[..] {
        [string] => {
            let addr = string.parse::<SocketAddr>().whatever_context(format!(
                "Invalid socket address `{string}` while parsing directive {}",
                directive.name
            ))?;
            Ok(Value::Addr(addr))
        }
        _ => whatever!(
            "Invalid number of arguments for directive: {}",
            directive.name
        ),
    }
}

// resolver udp
// resolver http
fn parse_resolver(directive: Directive<Nginx>) -> Result<Value> {
    match &directive.args[..] {
        [kind, resolver] => match kind.as_str() {
            "udp" => {
                whatever!("`udp` resolver is deprecated, please use `http` instead",);
                // let server_addr = resolver.parse::<SocketAddr>().whatever_context(format!(
                //     "Invalid socket address `{resolver}` whiling parsing udp resolver",
                // ))?;
                // Ok(Value::Resolver(Resolver::Udp { server_addr }))
            }
            "http" => {
                let base_url = resolver.parse::<Uri>().whatever_context(format!(
                    "Invalid base URL `{resolver}` whiling parsing http resolver",
                ))?;
                Ok(Value::Resolver(Resolver::Http { base_url }))
            }
            _ => whatever!("Unknown resolver kind: {kind}, expected `udp` or `http`"),
        },
        _ => whatever!(
            "Invalid number of arguments for directive: {}",
            directive.name
        ),
    }
}

fn parse_path(directive: Directive<Nginx>) -> Result<Value> {
    match &directive.args[..] {
        [string] => {
            let path = PathBuf::from(string);
            ensure_whatever!(path.exists(), "Path `{}` does not exist", path.display());
            Ok(Value::Path(path))
        }
        _ => whatever!(
            "Invalid number of arguments for directive: {}",
            directive.name
        ),
    }
}

fn parse_string_vec(directive: Directive<Nginx>) -> Result<Value> {
    Ok(Value::StringVec(directive.args))
}

fn parse_header(directive: Directive<Nginx>) -> Result<Value> {
    match &directive.args[..] {
        [name, value] => {
            let header_name = HeaderName::from_bytes(name.as_bytes()).whatever_context(format!(
                "Invalid header name `{name}` while parsing directive {}",
                directive.name
            ))?;
            let header_value =
                HeaderValue::from_bytes(value.as_bytes()).whatever_context(format!(
                    "Invalid header value `{value}` while parsing directive {}",
                    directive.name
                ))?;
            Ok(Value::Header(vec![(header_name, header_value, true)]))
        }
        _ => whatever!(
            "Invalid number of arguments for directive: {}",
            directive.name
        ),
    }
}

fn parse_header_always(directive: Directive<Nginx>) -> Result<Value> {
    match &directive.args[..] {
        [name, value] => {
            let header_name = HeaderName::from_bytes(name.as_bytes()).whatever_context(format!(
                "Invalid header name `{name}` while parsing directive {}",
                directive.name
            ))?;
            let header_value =
                HeaderValue::from_bytes(value.as_bytes()).whatever_context(format!(
                    "Invalid header value `{value}` while parsing directive {}",
                    directive.name
                ))?;
            Ok(Value::Header(vec![(header_name, header_value, false)]))
        }
        [name, value, always] => {
            ensure_whatever!(
                always == "always",
                "The third argument of directive {} must be `always`",
                directive.name
            );
            let header_name = HeaderName::from_bytes(name.as_bytes()).whatever_context(format!(
                "Invalid header name `{name}` while parsing directive {}",
                directive.name
            ))?;
            let header_value =
                HeaderValue::from_bytes(value.as_bytes()).whatever_context(format!(
                    "Invalid header value `{value}` while parsing directive {}",
                    directive.name
                ))?;
            Ok(Value::Header(vec![(header_name, header_value, true)]))
        }
        _ => whatever!(
            "Invalid number of arguments for directive: {}",
            directive.name
        ),
    }
}

fn parse_ssh_login(directive: Directive<Nginx>) -> Result<Value> {
    let auths = directive
        .args
        .iter()
        .map(|auth| {
            ensure_whatever!(
                auth == "basic" || auth == "ssl",
                "Invalid value for directive: {}",
                directive.name
            );
            Ok(auth.to_string())
        })
        .collect::<Result<Vec<_>>>()?;
    if auths.is_empty() {
        whatever!(
            "At least one authentication method is required for directive: {}",
            directive.name
        );
    }
    Ok(Value::StringVec(auths))
}

fn parse_ssh_ssl_user(directive: Directive<Nginx>) -> Result<Value> {
    match &directive.args[..] {
        [name, user] => Ok(Value::SshSslUser(vec![(
            name.to_string(),
            user.to_string(),
        )])),
        _ => whatever!(
            "Invalid number of arguments for directive: {}",
            directive.name
        ),
    }
}
