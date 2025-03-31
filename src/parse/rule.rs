//! Proxy rule configuration parser
//!
//! Handles parsing of proxy-related directives like proxy_pass and proxy_set_header

use std::collections::HashMap;

use misc_conf::{ast::Directive, nginx::Nginx};
use tracing::info;

use crate::error::{CustomError, Result};

/// Location 块的配置规则
///
/// 如果同时存在 proxy_pass root alias, 则只会解析第一个
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rule {
    ProxyPass(String),
    Root(String),
    Alias(String),
    ProxySetHeader(String, String),
    AddHeader(String, String),
    MimeTypes(HashMap<String, String>),
    DefaultType(String),
}

fn take_args<T, F>(rule: &Directive<Nginx>, extractor: F) -> Result<T>
where
    F: FnOnce(&[String]) -> Option<T>,
{
    extractor(&rule.args).ok_or_else(|| CustomError::InvalidArgs(rule.name.clone()))
}

pub fn parse_rule(rule: Directive<Nginx>) -> Result<Rule> {
    Ok(match rule.name.as_str() {
        "proxy_pass" => take_args(&rule, |args| match args {
            [arg] => Some(Rule::ProxyPass(arg.clone())),
            _ => None,
        })?,
        "root" => take_args(&rule, |args| match args {
            [arg] => Some(Rule::Root(arg.clone())),
            _ => None,
        })?,
        "alias" => take_args(&rule, |args| match args {
            [arg] => Some(Rule::Alias(arg.clone())),
            _ => None,
        })?,
        "proxy_set_header" => take_args(&rule, |args| match args {
            [name, value] => Some(Rule::ProxySetHeader(name.clone(), value.clone())),
            _ => None,
        })?,
        "add_header" => take_args(&rule, |args| match args {
            [name, value] => Some(Rule::AddHeader(name.clone(), value.clone())),
            _ => None,
        })?,
        unknown => {
            info!("unknown directive: {}", unknown);
            return Err(CustomError::UnknownDirective(unknown.to_string()));
        }
    })
}
