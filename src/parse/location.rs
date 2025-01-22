use std::collections::HashMap;

use misc_conf::{ast::Directive, nginx::Nginx};

use super::{
    pattern::{Pattern, parse_pattern},
    rule::{Rule, parse_rule},
    server::ServerType,
};
use crate::error::{CustomError, Result};

#[derive(Debug, Clone)]
pub struct Location {
    pub pattern: Pattern,
    pub rules: HashMap<String, Rule>,
}

pub fn parse_location(location: Directive<Nginx>, typ: ServerType) -> Result<Location> {
    let pattern = parse_pattern(&location.args)?;

    let mut rules = HashMap::new();
    if let Some(children) = location.children {
        for child in children {
            rules.insert(child.name.clone(), parse_rule(child)?);
        }
    }

    match typ {
        ServerType::Forward => {
            // 反向代理必须要有 proxy_pass 或 root
            let proxy_pass = rules.get("proxy_pass");
            let root = rules.get("root");

            match (proxy_pass, root) {
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
        }
        ServerType::Reverse => {
            // 正向代理必须要有 proxy_pass
            let proxy_pass = rules.get("proxy_pass");
            if proxy_pass.is_none() {
                return Err(CustomError::MissingConfig(
                    "location must have proxy_pass".to_string(),
                ));
            }
        }
    };

    Ok(Location { pattern, rules })
}
