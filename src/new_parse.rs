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
use tokio::sync::OnceCell;
use tracing::error;

use crate::parse::pattern::Pattern;

pub mod conf;
mod location;
mod pishoo;
mod proxy;
mod server;

type ParseFn = Box<dyn Fn(Directive<Nginx>) -> Result<ParseValue>>;

#[derive(Debug)]
pub enum ParseValue {
    String(String),
    StringVec(Vec<String>),
    StringMap(HashMap<String, String>),
    Boolean(bool),
    Addr(SocketAddr),
    Path(PathBuf),
    Header(Vec<(HeaderName, HeaderValue)>),
    HeaderAllways(Vec<(HeaderName, HeaderValue, bool)>),
    Location(Pattern, HashMap<String, ParseValue>),
    ValueMap(HashMap<String, ParseValue>),
    Nodes(Vec<Arc<ParseNode>>),
}

// 包含数据和父链接的节点结构
#[derive(Debug)]
pub struct ParseNode {
    // 使用RwLock实现内部可变性
    value: ParseValue,
    // Weak指针避免循环引用
    parent: OnceCell<Option<Weak<ParseNode>>>,
}

impl ParseNode {
    pub fn new(value: ParseValue) -> Self {
        assert!(matches!(
            value,
            ParseValue::ValueMap(..) | ParseValue::Location(..)
        ));

        Self {
            value,
            parent: OnceCell::new(),
        }
    }

    // 获取存活的父节点Arc
    pub fn parent(&self) -> Option<Arc<ParseNode>> {
        self.parent
            .get()
            .and_then(|opt_weak_ref| opt_weak_ref.as_ref())
            .and_then(|weak_parent| weak_parent.upgrade())
    }

    // 不可变访问节点值
    pub fn value(&self) -> &ParseValue {
        &self.value
    }

    fn set_parent(&self, parent: Option<Weak<ParseNode>>) {
        self.parent.set(parent).expect("Parent link set multiple times for the same node. This indicates a bug in the tree transformation logic.");
    }
}

pub fn parse(configure: &[u8], root: &Path) -> Result<Arc<ParseNode>> {
    let directives = Directive::<Nginx>::parse(configure)?;

    let processed_directives = directives
        .into_iter()
        .map(|mut directive| directive.resolve_include(root).map(|_| directive))
        .collect::<Result<Vec<_>>>()?;

    parse_conf(processed_directives).inspect_err(|e| error!("Error parsing directives: {}", e))
}

fn parse_string_map(directive: Directive<Nginx>) -> Result<ParseValue> {
    if let Some(children) = directive.children {
        let mut map = HashMap::new();
        for directive in children {
            let value = directive.name;
            for arg in directive.args {
                map.insert(arg, value.clone());
            }
        }
        return Ok(ParseValue::StringMap(map));
    }
    Ok(ParseValue::ValueMap(HashMap::new()))
}

fn parse_string(directive: Directive<Nginx>) -> Result<ParseValue> {
    match &directive.args[..] {
        [string] => Ok(ParseValue::String(string.to_string())),
        _ => Err(anyhow!(
            "Invalid number of arguments for directive: {}",
            directive.name
        )),
    }
}

fn parse_address(directive: Directive<Nginx>) -> Result<ParseValue> {
    match &directive.args[..] {
        [string] => {
            let addr = string.parse::<SocketAddr>()?;
            Ok(ParseValue::Addr(addr))
        }
        _ => Err(anyhow!(
            "Invalid number of arguments for directive: {}",
            directive.name
        )),
    }
}

fn parse_path(directive: Directive<Nginx>) -> Result<ParseValue> {
    match &directive.args[..] {
        [string] => {
            let path = PathBuf::from(string);
            if !path.exists() {
                return Err(anyhow!("Path does not exist: {}", string));
            }
            Ok(ParseValue::Path(path))
        }
        _ => Err(anyhow!(
            "Invalid number of arguments for directive: {}",
            directive.name
        )),
    }
}

fn parse_string_vec(directive: Directive<Nginx>) -> Result<ParseValue> {
    Ok(ParseValue::StringVec(directive.args))
}

fn parse_header(directive: Directive<Nginx>) -> Result<ParseValue> {
    match &directive.args[..] {
        [name, value] => {
            let header_name = HeaderName::from_bytes(name.as_bytes())?;
            let header_value = HeaderValue::from_bytes(value.as_bytes())?;
            Ok(ParseValue::Header(vec![(header_name, header_value)]))
        }
        _ => Err(anyhow!(
            "Invalid number of arguments for directive: {}",
            directive.name
        )),
    }
}

fn parse_header_allways(directive: Directive<Nginx>) -> Result<ParseValue> {
    match &directive.args[..] {
        [name, value] => {
            let header_name = HeaderName::from_bytes(name.as_bytes())?;
            let header_value = HeaderValue::from_bytes(value.as_bytes())?;
            Ok(ParseValue::HeaderAllways(vec![(
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
            Ok(ParseValue::HeaderAllways(vec![(
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
