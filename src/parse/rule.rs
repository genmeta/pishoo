use misc_conf::{ast::Directive, nginx::Nginx};
use tracing::info;

use crate::error::{CustomError, Result};

#[derive(Debug, Clone)]
pub enum Rule {
    Reverse(ReverseRule),
    Forward(ForwardRule),
}

#[derive(Debug, Clone, Default)]
pub struct ReverseRule {
    pub proxy_pass: Option<String>,
    pub root: Option<String>,
    pub proxy_set_header: Vec<(String, String)>,
    pub add_header: Vec<(String, String)>,
}

#[derive(Debug, Clone, Default)]
pub struct ForwardRule {
    pub proxy_pass: Option<String>,
    pub resolver: Option<Vec<String>>,
    pub proxy_set_header: Vec<(String, String)>,
    pub add_header: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleType {
    ProxyPass(String),
    Root(String),
    ProxySetHeader(String, String),
    AddHeader(String, String),
    Resolver(Vec<String>),
}

fn take_single_arg(args: Vec<String>, name: &str) -> Result<String> {
    match args.len() {
        0 => Err(CustomError::MissingArg(name.to_string())),
        1 => Ok(args.into_iter().next().unwrap()),
        _ => Err(CustomError::InvalidArgs(name.to_string())),
    }
}

fn take_two_args(args: Vec<String>, name: &str) -> Result<(String, String)> {
    match args.len() {
        2 => {
            let mut args = args.into_iter();
            let first = args.next().unwrap();
            let second = args.next().unwrap();
            Ok((first, second))
        }
        _ => Err(CustomError::InvalidArgs(name.to_string())),
    }
}

pub fn parse_rule(rule: Directive<Nginx>) -> Result<RuleType> {
    Ok(match rule.name.as_str() {
        "resolver" => {
            if rule.args.is_empty() {
                return Err(CustomError::MissingArg("resolver".into()));
            }
            RuleType::Resolver(rule.args)
        }
        "proxy_pass" => {
            let target = take_single_arg(rule.args, "proxy_pass")?;
            RuleType::ProxyPass(target)
        }
        "root" => {
            let root = take_single_arg(rule.args, "root")?;
            RuleType::Root(root)
        }
        "proxy_set_header" => {
            let (name, value) = take_two_args(rule.args, "proxy_set_header")?;
            RuleType::ProxySetHeader(name, value)
        }
        "add_header" => {
            let (name, value) = take_two_args(rule.args, "add_header")?;
            RuleType::AddHeader(name, value)
        }
        unknown => {
            info!("unknown directive: {}", unknown);
            return Err(CustomError::UnknownDirective(unknown.to_string()));
        }
    })
}
