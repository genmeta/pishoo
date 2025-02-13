use misc_conf::{ast::Directive, nginx::Nginx};

use super::{
    pattern::{Pattern, parse_pattern},
    rule::{Rule, RuleType, parse_rule_type},
};
use crate::error::{CustomError, Result};

#[derive(Debug, Clone)]
pub struct Location {
    pub pattern: Pattern,
    pub rule: Rule,
}

impl Location {
    pub fn parse(location: Directive<Nginx>) -> Result<Self> {
        let pattern = parse_pattern(&location.args)?;

        let mut rule = Rule::default();
        for rule_type in location.children.into_iter().flatten().map(parse_rule_type) {
            match rule_type? {
                RuleType::ProxyPass(proxy_pass) => rule.proxy_pass = proxy_pass,
                RuleType::ProxySetHeader(key, value) => rule.proxy_set_header.push((key, value)),
                RuleType::AddHeader(key, value) => {
                    rule.add_header.push((key, value));
                }
                RuleType::Resolver(resolver) => {
                    rule.resolver = resolver;
                }
            };
        }

        if rule.proxy_pass.is_empty() {
            return Err(CustomError::MissingConfig(
                "location must have proxy_pass".to_string(),
            ));
        }

        Ok(Self { pattern, rule })
    }
}
