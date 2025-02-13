use misc_conf::{ast::Directive, nginx::Nginx};
use tracing::info;

use crate::error::{CustomError, Result};

#[derive(Default, Debug, Clone)]
pub struct Rule {
    pub proxy_pass: String,
    pub resolver: Vec<String>,
    pub proxy_set_header: Vec<(String, String)>,
    pub add_header: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleType {
    ProxyPass(String),
    ProxySetHeader(String, String),
    AddHeader(String, String),
    Resolver(Vec<String>),
}

fn take_single_arg(rule: Directive<Nginx>) -> Result<String> {
    match &*rule.args {
        [] => Err(CustomError::MissingArg(rule.name.to_string())),
        [arg] => Ok(arg.clone()),
        _ => Err(CustomError::InvalidArgs(rule.name.to_string())),
    }
}

fn take_two_args(rule: Directive<Nginx>) -> Result<(String, String)> {
    match &*rule.args {
        [arg1, arg2] => Ok((arg1.clone(), arg2.clone())),
        _ => Err(CustomError::InvalidArgs(rule.name.to_string())),
    }
}

pub fn parse_rule_type(rule: Directive<Nginx>) -> Result<RuleType> {
    Ok(match rule.name.as_str() {
        "resolver" => {
            if rule.args.is_empty() {
                return Err(CustomError::MissingArg("resolver".into()));
            }
            RuleType::Resolver(rule.args)
        }
        "proxy_pass" => RuleType::ProxyPass(take_single_arg(rule)?),
        "proxy_set_header" => {
            let (name, value) = take_two_args(rule)?;
            RuleType::ProxySetHeader(name, value)
        }
        "add_header" => {
            let (name, value) = take_two_args(rule)?;
            RuleType::AddHeader(name, value)
        }
        unknown => {
            info!("unknown directive: {}", unknown);
            return Err(CustomError::UnknownDirective(unknown.to_string()));
        }
    })
}
