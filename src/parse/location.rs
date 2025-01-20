use misc_conf::{ast::Directive, nginx::Nginx};

use super::{
    pattern::{Pattern, parse_pattern},
    rule::{Rule, parse_rule},
};
use crate::error::Result;

#[derive(Debug, Clone)]
pub struct Location {
    pub pattern: Pattern,
    pub rules: Vec<Rule>,
}

pub fn parse_location(location: Directive<Nginx>) -> Result<Location> {
    let pattern = parse_pattern(&location.args)?;

    let mut rules = Vec::new();
    if let Some(children) = location.children {
        for child in children {
            rules.push(parse_rule(child)?);
        }
    }

    // TODO 检测 必须要有 proxy_pass 或者 root 之一

    Ok(Location { pattern, rules })
}
