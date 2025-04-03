//! Proxy rule configuration parser
//!
//! Handles parsing of proxy-related directives like proxy_pass and proxy_set_header

use std::{collections::HashMap, str::FromStr};

use http::{HeaderName, HeaderValue};
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
    ProxySetHeader(HeaderName, HeaderValue),
    AddHeader(HeaderName, HeaderValue, bool),
    MimeTypes(HashMap<String, String>),
    DefaultType(String),
    IndexFiles(Vec<String>),
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
        "proxy_set_header" => {
            let (header, value) = take_args(&rule, |args| match args {
                [name, value] => Some((name.clone(), value.clone())),
                _ => None,
            })?;
            let header = match HeaderName::from_str(&header) {
                Ok(header) => header,
                Err(_) => {
                    return Err(CustomError::InvalidArgs(format!(
                        "Invalid header name: {}",
                        header
                    )));
                }
            };
            let value = match HeaderValue::from_str(&value) {
                Ok(value) => value,
                Err(_) => {
                    return Err(CustomError::InvalidArgs(format!(
                        "Invalid header value: {}",
                        value
                    )));
                }
            };
            Rule::ProxySetHeader(header, value)
        }
        "add_header" => {
            let (header, value, always) = take_args(&rule, |args| match args {
                [name, value] => Some((name.clone(), value.clone(), false)),
                [name, value, always] if always == "always" => {
                    Some((name.clone(), value.clone(), true))
                }
                _ => None,
            })?;
            let header = match HeaderName::from_str(&header) {
                Ok(header) => header,
                Err(_) => {
                    return Err(CustomError::InvalidArgs(format!(
                        "Invalid header name: {}",
                        header
                    )));
                }
            };
            let value = match HeaderValue::from_str(&value) {
                Ok(value) => value,
                Err(_) => {
                    return Err(CustomError::InvalidArgs(format!(
                        "Invalid header value: {}",
                        value
                    )));
                }
            };
            Rule::AddHeader(header, value, always)
        }
        "types" => {
            let mut mime_types = HashMap::new();
            for directive in rule.children.unwrap_or_default() {
                if let Some((key, value)) = directive.args.split_first() {
                    mime_types.insert(key.clone(), value.join(" "));
                }
            }
            Rule::MimeTypes(mime_types)
        }
        "default_type" => take_args(&rule, |args| match args {
            [arg] => Some(Rule::DefaultType(arg.clone())),
            _ => None,
        })?,
        "index" => Rule::IndexFiles(rule.args),
        unknown => {
            info!("unknown directive: {}", unknown);
            return Err(CustomError::UnknownDirective(unknown.to_string()));
        }
    })
}
