use misc_conf::{ast::Directive, nginx::Nginx};

use super::{
    pattern::{Pattern, parse_pattern},
    rule::{ForwardRule, ForwardType, ReverseRule, Rule, RuleType, parse_rule},
    version::ServerType,
};
use crate::error::{CustomError, Result};

#[derive(Debug, Clone)]
pub struct Location {
    pub pattern: Pattern,
    pub rule: Rule,
}

pub fn parse_location(location: Directive<Nginx>, typ: ServerType) -> Result<Location> {
    let pattern = parse_pattern(&location.args)?;

    let mut rules = Vec::new();
    if let Some(children) = location.children {
        for child in children {
            rules.push(parse_rule(child)?);
        }
    }

    let rule = match typ {
        ServerType::Forward => {
            // 检测 必须要有 proxy_pass 或者 root 之一 并且不能同时存在
            let mut proxy_pass = None;
            let mut root = None;
            for rule in &rules {
                if let RuleType::ProxyPass(target) = rule {
                    proxy_pass = Some(target);
                } else if let RuleType::Root(target) = rule {
                    root = Some(target);
                }
            }

            let rule = match (proxy_pass, root) {
                (Some(target), None) => ForwardRule {
                    typ: ForwardType::Proxy(target.to_string()),
                    rules,
                },
                (None, Some(root)) => ForwardRule {
                    typ: ForwardType::Static(root.to_string()),
                    rules,
                },
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
            };
            Rule::Forward(rule)
        }
        ServerType::Reverse => {
            // 检测 必须要有 proxy_pass
            let mut proxy_pass = None;
            for rule in &rules {
                if let RuleType::ProxyPass(target) = rule {
                    proxy_pass = Some(target);
                }
            }

            if proxy_pass.is_none() {
                return Err(CustomError::MissingConfig(
                    "location must have proxy_pass".to_string(),
                ));
            }

            Rule::Reverse(ReverseRule { rules })
        }
    };

    Ok(Location { pattern, rule })
}
