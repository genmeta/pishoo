use std::{
    collections::HashMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Weak},
};

use anyhow::{Result, anyhow};
use conf::parse_conf;
use http::{HeaderName, HeaderValue};
use misc_conf::{
    ast::{Directive, DirectiveTrait},
    nginx::Nginx,
};
use pattern::Pattern;
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
    StringVec(Vec<String>),
    StringMap(HashMap<String, String>),
    Boolean(bool),
    Addr(SocketAddr),
    Path(PathBuf),
    Header(Vec<(HeaderName, HeaderValue)>),
    HeaderAllways(Vec<(HeaderName, HeaderValue, bool)>),
    Pattern(Pattern, HashMap<String, Value>),
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
        while let Some(parent) = current_node.parent() {
            if current_node.value().get(key).is_some() {
                return Some(Arc::clone(&current_node));
            }
            current_node = parent;
        }
        None
    }
}

pub fn parse(configure: &[u8], root: &Path) -> Result<Arc<Node>> {
    let directives = Directive::<Nginx>::parse(configure)?;

    let processed_directives = directives
        .into_iter()
        .map(|mut directive| directive.resolve_include(root).map(|_| directive))
        .collect::<Result<Vec<_>>>()?;

    parse_conf(processed_directives).inspect_err(|e| error!("Error parsing directives: {}", e))
}

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

fn parse_string(directive: Directive<Nginx>) -> Result<Value> {
    match &directive.args[..] {
        [string] => Ok(Value::String(string.to_string())),
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
            Ok(Value::Header(vec![(header_name, header_value)]))
        }
        _ => Err(anyhow!(
            "Invalid number of arguments for directive: {}",
            directive.name
        )),
    }
}

fn parse_header_allways(directive: Directive<Nginx>) -> Result<Value> {
    match &directive.args[..] {
        [name, value] => {
            let header_name = HeaderName::from_bytes(name.as_bytes())?;
            let header_value = HeaderValue::from_bytes(value.as_bytes())?;
            Ok(Value::HeaderAllways(vec![(
                header_name,
                header_value,
                false,
            )]))
        }
        [name, value, always] => {
            if always.as_str() != "always" {
                return Err(anyhow!("Invalid argument for always: {}", always));
            }
            let header_name = HeaderName::from_bytes(name.as_bytes())?;
            let header_value = HeaderValue::from_bytes(value.as_bytes())?;
            Ok(Value::HeaderAllways(vec![(
                header_name,
                header_value,
                true,
            )]))
        }
        _ => Err(anyhow!(
            "Invalid number of arguments for directive: {}",
            directive.name
        )),
    }
}
