use misc_conf::{ast::Directive, nginx::Nginx};

use super::{
    pattern::{Pattern, parse_pattern},
    rule::{ForwardRule, ReverseRule, Rule, RuleType, parse_rule},
    server::ServerType,
};
use crate::error::{CustomError, Result};

#[derive(Debug, Clone)]
pub struct Location {
    pub pattern: Pattern,
    pub rules: Rule,
}

pub fn parse_location(location: Directive<Nginx>, typ: ServerType) -> Result<Location> {
    let pattern = parse_pattern(&location.args)?;

    let mut rules = Vec::new();
    if let Some(children) = location.children {
        for child in children {
            rules.push(parse_rule(child)?);
        }
    }

    let rules = match typ {
        ServerType::Forward => {
            let mut forward_rules = ForwardRule::default();

            for rule in rules {
                match rule {
                    RuleType::ProxyPass(proxy_pass) => {
                        if forward_rules.proxy_pass.is_none() {
                            forward_rules.proxy_pass = Some(proxy_pass)
                        }
                    }
                    RuleType::Root(root) => {
                        if forward_rules.root.is_none() {
                            forward_rules.root = Some(root)
                        }
                    }
                    RuleType::ProxySetHeader(key, value) => {
                        forward_rules.proxy_set_header.push((key, value))
                    }
                    RuleType::AddHeader(key, value) => forward_rules.add_header.push((key, value)),
                    rule => {
                        return Err(CustomError::InvalidConfig(format!(
                            "forward location not support this rule type: {rule:?}"
                        )));
                    }
                }
            }

            // 反向代理必须要有 proxy_pass 或 root
            match (&forward_rules.proxy_pass, &forward_rules.root) {
                (Some(_), Some(_)) => {
                    return Err(CustomError::InvalidConfig(
                        "location must have only one of proxy_pass or root".to_string(),
                    ));
                }
                (None, None) => {
                    return Err(CustomError::MissingConfig(
                        "location must have proxy_pass or root".to_string(),
                    ));
                }
                _ => {}
            };
            Rule::Forward(forward_rules)
        }
        ServerType::Reverse => {
            let mut reverse_rules = ReverseRule::default();

            for rule in rules {
                match rule {
                    RuleType::ProxyPass(proxy_pass) => {
                        if reverse_rules.proxy_pass.is_none() {
                            reverse_rules.proxy_pass = Some(proxy_pass)
                        }
                    }
                    RuleType::Resolver(resolver) => {
                        if reverse_rules.resolver.is_none() {
                            reverse_rules.resolver = Some(resolver)
                        }
                    }
                    RuleType::ProxySetHeader(key, value) => {
                        reverse_rules.proxy_set_header.push((key, value))
                    }
                    RuleType::AddHeader(key, value) => reverse_rules.add_header.push((key, value)),
                    rule => {
                        return Err(CustomError::InvalidConfig(format!(
                            "reverse location not support this rule type: {rule:?}"
                        )));
                    }
                }
            }

            if reverse_rules.proxy_pass.is_none() {
                return Err(CustomError::MissingConfig(
                    "reverse location must have proxy_pass".to_string(),
                ));
            }
            Rule::Reverse(reverse_rules)
        }
    };

    Ok(Location { pattern, rules })
}
