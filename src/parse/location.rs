//! Nginx location block parser
//!
//! Handles parsing of location directives and their configuration rules

use misc_conf::{ast::Directive, nginx::Nginx};

use super::{
    pattern::{Pattern, parse_pattern},
    rule::{Rule, parse_rule},
};
use crate::error::{CustomError, Result};

#[derive(Debug, Clone)]
pub enum Location {
    Proxy(String, Vec<Rule>),
    Root(String, Vec<Rule>),
    Alias(String, Vec<Rule>),
}

impl Location {
    pub fn parse(location: Directive<Nginx>) -> Result<(Pattern, Self)> {
        let pattern = parse_pattern(&location.args)?;

        let mut type_rule = None;
        let mut nomal_rule = vec![];

        for rule in location.children.into_iter().flatten().flat_map(parse_rule) {
            match rule {
                Rule::ProxyPass(_) | Rule::Root(_) | Rule::Alias(_) => {
                    if type_rule.is_none() {
                        type_rule = Some(rule);
                    }
                }
                _ => {
                    nomal_rule.push(rule);
                }
            }
        }

        match type_rule {
            Some(rule) => match rule {
                Rule::ProxyPass(proxy_pass) => {
                    Ok((pattern, Location::Proxy(proxy_pass, nomal_rule)))
                }
                Rule::Root(root) => Ok((pattern, Location::Root(root, nomal_rule))),
                Rule::Alias(alias) => Ok((pattern, Location::Alias(alias, nomal_rule))),
                _ => Err(CustomError::InvalidConfig(
                    "location must have proxy_pass root or alias".to_string(),
                )),
            },
            None => Err(CustomError::MissingConfig(
                "location must have proxy_pass root or alias".to_string(),
            )),
        }
    }
}
