use misc_conf::{ast::Directive, nginx::Nginx};
use tracing::info;

use crate::error::{CustomError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleType {
    ProxyPass(String),
    Root(String),
    ProxySetHeader(String, String),
    AddHeader(String, String),
    Resolver(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardRule {
    pub typ: ForwardType,
    pub rules: Vec<RuleType>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForwardType {
    Proxy(String),
    Static(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReverseRule {
    pub rules: Vec<RuleType>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rule {
    Forward(ForwardRule),
    Reverse(ReverseRule),
}

// proxy_pass $scheme://$http_host$request_uri;

pub fn parse_rule(rule: Directive<Nginx>) -> Result<RuleType> {
    match rule.name.as_str() {
        "resolver" => Ok(RuleType::Resolver(
            rule.args.into_iter().map(|s| s.to_string()).collect(),
        )),
        "proxy_pass" => rule
            .args
            .first()
            .map(|target| RuleType::ProxyPass(target.clone()))
            .ok_or_else(|| CustomError::MissingArg("proxy_pass".to_string())),
        "root" => rule
            .args
            .first()
            .map(|root| RuleType::Root(root.clone()))
            .ok_or_else(|| CustomError::MissingArg("root".to_string())),
        "proxy_set_header" => match &rule.args[..] {
            [name, value] => Ok(RuleType::ProxySetHeader(
                name.to_string(),
                value.to_string(),
            )),
            _ => Err(CustomError::InvalidArgs("proxy_set_header".to_string())),
        },
        "add_header" => match &rule.args[..] {
            [name, value] => Ok(RuleType::AddHeader(name.to_string(), value.to_string())),
            _ => Err(CustomError::InvalidArgs("add_header".to_string())),
        },
        _ => {
            info!("unknown directive: {}", rule.name);
            Err(CustomError::UnknownDirective(rule.name))
        }
    }
}
