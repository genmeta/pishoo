use misc_conf::{ast::Directive, nginx::Nginx};
use tracing::info;

use crate::error::{CustomError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rule {
    ProxyPass(String),
    Root(String),
    // TODO 应该是 Vec<(String, String)>
    ProxySetHeader(String, String),
    // TODO 应该是 Vec<(String, String)>
    AddHeader(String, String),
    Resolver(Vec<String>),
}

// proxy_pass $scheme://$http_host$request_uri;

pub fn parse_rule(rule: Directive<Nginx>) -> Result<Rule> {
    let rule = match rule.name.as_str() {
        "resolver" => {
            let rule = rule.args.into_iter().collect();
            Rule::Resolver(rule)
        }
        "proxy_pass" => {
            let target = rule
                .args
                .first()
                .map(String::from)
                .ok_or_else(|| CustomError::MissingArg("proxy_pass".to_string()))?;
            Rule::ProxyPass(target)
        }
        "root" => {
            let root = rule
                .args
                .first()
                .map(String::from)
                .ok_or_else(|| CustomError::MissingArg("root".to_string()))?;
            Rule::Root(root)
        }
        "proxy_set_header" => match &rule.args[..] {
            [name, value] => Rule::ProxySetHeader(name.to_string(), value.to_string()),
            _ => return Err(CustomError::InvalidArgs("proxy_set_header".to_string())),
        },
        "add_header" => match &rule.args[..] {
            [name, value] => Rule::AddHeader(name.to_string(), value.to_string()),
            _ => return Err(CustomError::InvalidArgs("add_header".to_string())),
        },
        _ => {
            info!("unknown directive: {}", rule.name);
            return Err(CustomError::UnknownDirective(rule.name));
        }
    };
    Ok(rule)
}
