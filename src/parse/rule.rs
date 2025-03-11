//! Proxy rule configuration parser
//!
//! Handles parsing of proxy-related directives like proxy_pass and proxy_set_header

use misc_conf::{ast::Directive, nginx::Nginx};
use tracing::info;

use crate::error::{CustomError, Result};

#[derive(Default, Debug, Clone)]
pub struct Rule {
    pub proxy_pass: String,
    pub proxy_set_header: Vec<(String, String)>,
    pub add_header: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleType {
    ProxyPass(String),
    ProxySetHeader(String, String),
    AddHeader(String, String),
}

fn take_args<T, F>(rule: &Directive<Nginx>, extractor: F) -> Result<T>
where
    F: FnOnce(&[String]) -> Option<T>,
{
    extractor(&rule.args).ok_or_else(|| CustomError::InvalidArgs(rule.name.clone()))
}

pub fn parse_rule_type(rule: Directive<Nginx>) -> Result<RuleType> {
    Ok(match rule.name.as_str() {
        "proxy_pass" => take_args(&rule, |args| match args {
            [arg] => Some(RuleType::ProxyPass(arg.clone())),
            _ => None,
        })?,
        "proxy_set_header" => take_args(&rule, |args| match args {
            [name, value] => Some(RuleType::ProxySetHeader(name.clone(), value.clone())),
            _ => None,
        })?,
        "add_header" => take_args(&rule, |args| match args {
            [name, value] => Some(RuleType::AddHeader(name.clone(), value.clone())),
            _ => None,
        })?,
        unknown => {
            info!("unknown directive: {}", unknown);
            return Err(CustomError::UnknownDirective(unknown.to_string()));
        }
    })
}
