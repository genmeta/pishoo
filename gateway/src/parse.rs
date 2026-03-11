use std::{
    collections::HashMap,
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
use snafu::{OptionExt, ResultExt, ensure_whatever, whatever};
use tokio::sync::OnceCell;

use crate::error::Whatever;

thread_local! {
    pub(crate) static CONFIG_ROOT: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

pub mod conf;
mod location;
pub mod pattern;
mod pishoo;
mod proxy;
mod server;

type Result<T, E = Whatever> = std::result::Result<T, E>;

type ParseFn = fn(Directive<Nginx>) -> Result<Value>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServerName {
    pub name: String,
}

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
    Addr(SocketAddr),
    AddrVec(Vec<SocketAddr>),
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

// TLS identity configuration for clients/servers that need certificates
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerIdentity {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    pub server_name: String,
    pub server_id: u8,
}

pub fn server_id_or_default(node: &Node) -> u8 {
    match node.get("server_id") {
        Some(Value::ServerId(id)) => *id,
        _ => 0,
    }
}

pub fn server_identity(node: &Node, server_name: String) -> Option<ServerIdentity> {
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

pub fn optional_server_identity(node: &Node, server_name_key: &str) -> Option<ServerIdentity> {
    let Some(Value::String(server_name)) = node.get(server_name_key) else {
        return None;
    };

    server_identity(node, server_name.clone())
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
            IfaceRange::External | IfaceRange::Internal => {
                tracing::warn!(
                    "IfaceRange::External/Internal not yet implemented, treating as non-match"
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
        use gm_quic::qbase::net::*;
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

    pub fn get_value_recursive(self: &Arc<Self>, key: &str) -> Option<Value> {
        self.backtrack_node(key).and_then(|n| n.get(key).cloned())
    }

    pub fn get_bool(self: &Arc<Self>, key: &str) -> Option<bool> {
        match self.get_value_recursive(key) {
            Some(Value::Boolean(b)) => Some(b),
            _ => None,
        }
    }

    pub fn get_str_parsed<T: FromStr>(self: &Arc<Self>, key: &str) -> Option<T> {
        match self.get_value_recursive(key) {
            Some(Value::String(s)) => s.parse().ok(),
            _ => None,
        }
    }

    pub fn get_string_vec(self: &Arc<Self>, key: &str) -> Option<Vec<String>> {
        match self.get_value_recursive(key) {
            Some(Value::StringVec(v)) => Some(v),
            _ => None,
        }
    }

    pub fn get_types(self: &Arc<Self>, key: &str) -> Option<HashMap<String, HeaderValue>> {
        match self.get_value_recursive(key) {
            Some(Value::Types(v)) => Some(v),
            _ => None,
        }
    }

    pub fn get_header_value(self: &Arc<Self>, key: &str) -> Option<HeaderValue> {
        match self.get_value_recursive(key) {
            Some(Value::HeaderValue(v)) => Some(v.clone()),
            _ => None,
        }
    }
}

pub fn parse(configure: &[u8], root: Option<&Path>) -> Result<Arc<Node>> {
    CONFIG_ROOT.with(|r| *r.borrow_mut() = root.map(|p| p.to_path_buf()));

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
                whatever!("Unknown directive `{name}`",)
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
                    let Value::SshSslUser(exist_users) = values
                        .entry(name)
                        .or_insert_with(|| Value::SshSslUser(vec![]))
                    else {
                        unreachable!("Unexpected value type, should be `SshSslUser`");
                    };
                    exist_users.extend(users);
                }
                Value::Addr(addr) => {
                    values
                        .entry(name)
                        .and_modify(|v| match v {
                            Value::Addr(old_addr) => *v = Value::AddrVec(vec![*old_addr, addr]),
                            Value::AddrVec(vec) => vec.push(addr),
                            _ => unreachable!("Unexpected value type for Addr aggregation"),
                        })
                        .or_insert(Value::Addr(addr));
                }
                value => _ = values.insert(name, value),
            }
        }
        Ok(values)
    }
}

#[allow(dead_code)]
pub(crate) fn parse_string_map(directive: Directive<Nginx>) -> Result<Value> {
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

pub(crate) fn parse_boolean(directive: Directive<Nginx>) -> Result<Value> {
    match &directive.args[..] {
        [flag] => match flag.as_str() {
            "on" => Ok(Value::Boolean(true)),
            "off" => Ok(Value::Boolean(false)),
            _ => whatever!("Invalid boolean value `{flag}`, expected `on` or `off`"),
        },
        _ => whatever!(
            "Invalid number of arguments for directive: {}",
            directive.name
        ),
    }
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

pub(crate) fn parse_string(directive: Directive<Nginx>) -> Result<Value> {
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

            let scheme = uri
                .scheme_str()
                .whatever_context::<_, Whatever>("Missing scheme in proxy_pass URI")
                .whatever_context(format!(
                    "Invalid URI `{s}` while parsing directive {}",
                    directive.name
                ))?;

            ensure_whatever!(
                matches!(scheme, "http" | "https"),
                "Invalid proxy_pass scheme `{scheme}`, expected `http` or `https`"
            );

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
            // Check if iface is actually a list of addresses
            if iface.contains(',') || iface.parse::<SocketAddr>().is_ok() {
                let addrs = iface
                    .split(',')
                    .map(|s| {
                        s.trim().parse::<SocketAddr>().whatever_context(format!(
                            "Invalid socket address `{s}` while parsing directive {}",
                            directive.name
                        ))
                    })
                    .collect::<Result<Vec<SocketAddr>, Whatever>>();

                if let Ok(addrs) = addrs {
                    return Ok(Value::Listen(vec![Listens {
                        range: IfaceRange::All,
                        families: IpFamilies::Dual,
                        port: 0,
                        specific_addrs: Some(addrs),
                    }]));
                }
            }

            // 单个参数 只能是网卡名, 省略了 families 和端口的情况
            Ok(Value::Listen(vec![Listens {
                range: IfaceRange::from(iface.as_str()),
                families: IpFamilies::default(),
                port: 0,
                specific_addrs: None,
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
                    specific_addrs: None,
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
                        specific_addrs: None,
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
                specific_addrs: None,
            }]))
        }
        _ => whatever!(
            "Invalid number of arguments for directive: {}",
            directive.name
        ),
    }
}

pub(crate) fn parse_address(directive: Directive<Nginx>) -> Result<Value> {
    match &directive.args[..] {
        [string] => {
            if string.contains(',') {
                let addrs = string
                    .split(',')
                    .map(|s| {
                        s.trim().parse::<SocketAddr>().whatever_context(format!(
                            "Invalid socket address `{s}` while parsing directive {}",
                            directive.name
                        ))
                    })
                    .collect::<Result<Vec<SocketAddr>, Whatever>>()?;
                Ok(Value::AddrVec(addrs))
            } else {
                let addr = string.parse::<SocketAddr>().whatever_context(format!(
                    "Invalid socket address `{string}` while parsing directive {}",
                    directive.name
                ))?;
                Ok(Value::Addr(addr))
            }
        }
        _ => whatever!(
            "Invalid number of arguments for directive: {}",
            directive.name
        ),
    }
}

fn parse_resolver(directive: Directive<Nginx>) -> Result<Value> {
    match &directive.args[..] {
        [kind, resolver] => match kind.as_str() {
            "udp" => {
                whatever!("`udp` resolver is deprecated, please use `h3` instead",)
            }
            "http" => {
                whatever!("`http` resolver is deprecated, please use `h3` instead",)
            }
            "h3" => {
                let base_url = resolver.parse::<Uri>().whatever_context(format!(
                    "Invalid base URL `{resolver}` whiling parsing h3 resolver",
                ))?;

                Ok(Value::Resolver(base_url))
            }
            _ => whatever!("Unknown resolver kind: {kind}, expected `h3`"),
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

pub(crate) fn parse_string_vec(directive: Directive<Nginx>) -> Result<Value> {
    Ok(Value::StringVec(directive.args))
}

pub(crate) fn parse_server_name(directive: Directive<Nginx>) -> Result<Value> {
    let names: Vec<ServerName> = directive
        .args
        .into_iter()
        .map(|name| ServerName { name })
        .collect();
    Ok(Value::ServerName(names))
}

pub(crate) fn parse_server_id(directive: Directive<Nginx>) -> Result<Value> {
    match &directive.args[..] {
        [id_str] => {
            let id = id_str.parse::<u8>().whatever_context(format!(
                "Invalid server ID `{id_str}` while parsing directive {}",
                directive.name
            ))?;
            Ok(Value::ServerId(id))
        }
        _ => whatever!(
            "Invalid number of arguments for directive: {}",
            directive.name
        ),
    }
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

#[cfg(test)]
mod tests {
    use std::{
        env,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    fn create_temp_file(prefix: &str) -> PathBuf {
        let path = env::temp_dir().join(format!(
            "gateway_{prefix}_{}_{}.pem",
            std::process::id(),
            NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, "dummy").expect("Failed to create temp file");
        path
    }

    fn cleanup_temp_files(paths: &[&Path]) {
        for path in paths {
            let _ = std::fs::remove_file(path);
        }
    }

    fn build_proxy_conf(server_cert: &Path, server_key: &Path, location_body: &str) -> String {
        format!(
            r#"
pishoo {{
    server {{
        listen all 5378;
        server_name example.com;
        ssl_certificate {};
        ssl_certificate_key {};
        location /api {{
            {}
        }}
    }}
}}
"#,
            server_cert.display(),
            server_key.display(),
            location_body
        )
    }

    /// 测试：解析新的 server_id 配置格式
    #[test]
    fn test_parse_server_with_server_id() {
        let conf = r#"
pishoo {
    server {
        listen all 5378;
        server_name example.com;
        server_id 1;
        ssl_certificate /tmp/test_cert.pem;
        ssl_certificate_key /tmp/test_key.pem;
    }
}
"#;
        // 创建临时证书文件用于测试
        std::fs::write("/tmp/test_cert.pem", "dummy cert").expect("Failed to create test cert");
        std::fs::write("/tmp/test_key.pem", "dummy key").expect("Failed to create test key");

        let result = parse(conf.as_bytes(), None);

        // 清理临时文件
        let _ = std::fs::remove_file("/tmp/test_cert.pem");
        let _ = std::fs::remove_file("/tmp/test_key.pem");

        let root = result.expect("配置解析失败");
        let pishoo = root
            .get("pishoo")
            .and_then(|v| {
                if let Value::Nodes(n) = v {
                    n.first().cloned()
                } else {
                    None
                }
            })
            .expect("未找到 pishoo 块");

        let servers = match pishoo.get("server") {
            Some(Value::Nodes(nodes)) => nodes,
            _ => panic!("未找到 server 配置块"),
        };
        assert_eq!(servers.len(), 1);

        let server = &servers[0];

        // 验证 server_name
        match server.get("server_name") {
            Some(Value::ServerName(names)) => {
                assert_eq!(names.len(), 1);
                assert_eq!(names[0].name, "example.com");
            }
            _ => panic!("server_name 解析失败"),
        }

        // 验证 server_id
        match server.get("server_id") {
            Some(Value::ServerId(id)) => assert_eq!(*id, 1),
            _ => panic!("server_id 解析失败"),
        }
    }

    /// 测试：解析多个 server 配置，每个有不同的 server_id
    #[test]
    fn test_parse_multiple_servers_with_different_ids() {
        let conf = r#"
pishoo {
    server {
        listen all 5378;
        server_name main.example.com;
        server_id 0;
        ssl_certificate /tmp/test_cert1.pem;
        ssl_certificate_key /tmp/test_key1.pem;
    }
    server {
        listen all 5379;
        server_name backup.example.com;
        server_id 1;
        ssl_certificate /tmp/test_cert2.pem;
        ssl_certificate_key /tmp/test_key2.pem;
    }
}
"#;
        // 创建临时证书文件用于测试
        std::fs::write("/tmp/test_cert1.pem", "dummy cert 1")
            .expect("Failed to create test cert 1");
        std::fs::write("/tmp/test_key1.pem", "dummy key 1").expect("Failed to create test key 1");
        std::fs::write("/tmp/test_cert2.pem", "dummy cert 2")
            .expect("Failed to create test cert 2");
        std::fs::write("/tmp/test_key2.pem", "dummy key 2").expect("Failed to create test key 2");

        let result = parse(conf.as_bytes(), None);

        // 清理临时文件
        let _ = std::fs::remove_file("/tmp/test_cert1.pem");
        let _ = std::fs::remove_file("/tmp/test_key1.pem");
        let _ = std::fs::remove_file("/tmp/test_cert2.pem");
        let _ = std::fs::remove_file("/tmp/test_key2.pem");

        let root = result.expect("配置解析失败");
        let pishoo = root
            .get("pishoo")
            .and_then(|v| {
                if let Value::Nodes(n) = v {
                    n.first().cloned()
                } else {
                    None
                }
            })
            .expect("未找到 pishoo 块");

        let servers = match pishoo.get("server") {
            Some(Value::Nodes(nodes)) => nodes,
            _ => panic!("未找到 server 配置块"),
        };
        assert_eq!(servers.len(), 2);

        // 验证第一个 server
        let server1 = &servers[0];
        match server1.get("server_name") {
            Some(Value::ServerName(names)) => assert_eq!(names[0].name, "main.example.com"),
            _ => panic!("第一个 server 的 server_name 解析失败"),
        }
        match server1.get("server_id") {
            Some(Value::ServerId(id)) => assert_eq!(*id, 0),
            _ => panic!("第一个 server 的 server_id 解析失败"),
        }

        // 验证第二个 server
        let server2 = &servers[1];
        match server2.get("server_name") {
            Some(Value::ServerName(names)) => assert_eq!(names[0].name, "backup.example.com"),
            _ => panic!("第二个 server 的 server_name 解析失败"),
        }
        match server2.get("server_id") {
            Some(Value::ServerId(id)) => assert_eq!(*id, 1),
            _ => panic!("第二个 server 的 server_id 解析失败"),
        }
    }

    /// 测试：server 配置缺少 server_id 时的默认行为
    #[test]
    fn test_parse_server_without_server_id() {
        use std::env;
        let temp_dir = env::temp_dir();
        let cert_path = temp_dir.join("test_cert_no_id.pem");
        let key_path = temp_dir.join("test_key_no_id.pem");

        let conf = format!(
            r#"
pishoo {{
    server {{
        listen all 5378;
        server_name example.com;
        ssl_certificate {};
        ssl_certificate_key {};
    }}
}}
"#,
            cert_path.display(),
            key_path.display()
        );

        // 创建临时证书文件用于测试
        std::fs::write(&cert_path, "dummy cert").expect("Failed to create test cert");
        std::fs::write(&key_path, "dummy key").expect("Failed to create test key");

        let result = parse(conf.as_bytes(), None);

        // 清理临时文件
        let _ = std::fs::remove_file(&cert_path);
        let _ = std::fs::remove_file(&key_path);

        let root = result.expect("配置解析失败");
        let pishoo = root
            .get("pishoo")
            .and_then(|v| {
                if let Value::Nodes(n) = v {
                    n.first().cloned()
                } else {
                    None
                }
            })
            .expect("未找到 pishoo 块");

        let servers = match pishoo.get("server") {
            Some(Value::Nodes(nodes)) => nodes,
            _ => panic!("未找到 server 配置块"),
        };
        assert_eq!(servers.len(), 1);

        let server = &servers[0];

        // 验证 server_name
        match server.get("server_name") {
            Some(Value::ServerName(names)) => assert_eq!(names[0].name, "example.com"),
            _ => panic!("server_name 解析失败"),
        }

        // 验证没有 server_id 时应该返回 None
        match server.get("server_id") {
            None => {} // 这是期望的行为
            Some(_) => panic!("不应该有 server_id 字段"),
        }
    }

    /// 测试：解析 DNS resolver 和 publisher 配置
    #[test]
    fn test_parse_dns_resolver_and_publisher() {
        use std::env;
        let temp_dir = env::temp_dir();
        let cert_path = temp_dir.join("test_cert_dns.pem");
        let key_path = temp_dir.join("test_key_dns.pem");

        let conf = format!(
            r#"
pishoo {{
    server {{
        listen all 5378;
        server_name example.com;
        dns h3 https://dns.example.com/dns-query;
        ssl_certificate {};
        ssl_certificate_key {};
    }}
}}
"#,
            cert_path.display(),
            key_path.display()
        );

        // 创建临时证书文件用于测试
        std::fs::write(&cert_path, "dummy cert").expect("Failed to create test cert");
        std::fs::write(&key_path, "dummy key").expect("Failed to create test key");

        let result = parse(conf.as_bytes(), None);

        // 清理临时文件
        let _ = std::fs::remove_file(&cert_path);
        let _ = std::fs::remove_file(&key_path);

        let root = result.expect("配置解析失败");
        let pishoo = root
            .get("pishoo")
            .and_then(|v| {
                if let Value::Nodes(n) = v {
                    n.first().cloned()
                } else {
                    None
                }
            })
            .expect("未找到 pishoo 块");

        let servers = match pishoo.get("server") {
            Some(Value::Nodes(nodes)) => nodes,
            _ => panic!("未找到 server 配置块"),
        };
        assert_eq!(servers.len(), 1);

        let server = &servers[0];

        // 验证 dns 配置
        match server.get("dns") {
            Some(Value::Resolver(resolver)) => {
                assert_eq!(resolver.to_string(), "https://dns.example.com/dns-query");
            }
            _ => panic!("dns 解析失败"),
        }
    }

    #[test]
    fn test_parse_proxy_pass_rejects_unsupported_scheme() {
        let server_cert = create_temp_file("server_cert_scheme");
        let server_key = create_temp_file("server_key_scheme");
        let conf = build_proxy_conf(
            &server_cert,
            &server_key,
            "proxy_pass ftp://backend.example.com;",
        );

        let result = parse(conf.as_bytes(), None);

        cleanup_temp_files(&[&server_cert, &server_key]);

        assert!(result.is_err(), "unsupported proxy_pass scheme should fail");
    }

    #[test]
    fn test_parse_https_proxy_with_optional_trusted_certificate() {
        let server_cert = create_temp_file("server_cert_https");
        let server_key = create_temp_file("server_key_https");
        let trusted_ca = create_temp_file("trusted_ca_https");
        let conf = build_proxy_conf(
            &server_cert,
            &server_key,
            &format!(
                "proxy_pass https://backend.example.com; proxy_ssl_trusted_certificate {};",
                trusted_ca.display()
            ),
        );

        let result = parse(conf.as_bytes(), None);

        cleanup_temp_files(&[&server_cert, &server_key, &trusted_ca]);

        assert!(result.is_ok(), "https proxy with trusted CA should parse");
    }

    #[test]
    fn test_parse_proxy_ssl_certificate_requires_matching_key() {
        let server_cert = create_temp_file("server_cert_pair");
        let server_key = create_temp_file("server_key_pair");
        let proxy_client_cert = create_temp_file("proxy_client_cert_pair");
        let conf = build_proxy_conf(
            &server_cert,
            &server_key,
            &format!(
                "proxy_pass https://backend.example.com; proxy_ssl_certificate {};",
                proxy_client_cert.display()
            ),
        );

        let result = parse(conf.as_bytes(), None);

        cleanup_temp_files(&[&server_cert, &server_key, &proxy_client_cert]);

        assert!(
            result.is_err(),
            "proxy_ssl_certificate without key should fail"
        );
    }

    #[test]
    fn test_parse_http_proxy_allows_proxy_ssl_directives() {
        let server_cert = create_temp_file("server_cert_http");
        let server_key = create_temp_file("server_key_http");
        let proxy_client_cert = create_temp_file("proxy_client_cert_http");
        let proxy_client_key = create_temp_file("proxy_client_key_http");
        let trusted_ca = create_temp_file("trusted_ca_http");
        let conf = build_proxy_conf(
            &server_cert,
            &server_key,
            &format!(
                "proxy_pass http://backend.example.com; proxy_ssl_certificate {}; proxy_ssl_certificate_key {}; proxy_ssl_trusted_certificate {};",
                proxy_client_cert.display(),
                proxy_client_key.display(),
                trusted_ca.display()
            ),
        );

        let result = parse(conf.as_bytes(), None);

        cleanup_temp_files(&[
            &server_cert,
            &server_key,
            &proxy_client_cert,
            &proxy_client_key,
            &trusted_ca,
        ]);

        assert!(
            result.is_ok(),
            "http proxy should allow proxy_ssl directives to remain optional"
        );
    }
}
