use std::path::Path;

use gateway::{Gateway, parse_gateway};
use misc_conf::{
    ast::{Directive, DirectiveTrait},
    nginx::Nginx,
};

use crate::error::{CustomError, Result};

pub mod gateway;
pub mod location;
pub mod pattern;
pub mod proxy;
pub mod router;
pub mod rule;
pub mod server;

pub fn parse_conf(configure: &[u8], root: &Path) -> Result<Gateway> {
    let directives = Directive::<Nginx>::parse(configure)
        .map_err(|e| CustomError::InvalidDirective(format!("Initial parse error: {}", e)))?;

    let processed_directives = directives
        .into_iter()
        .map(|mut directive| {
            directive
                .resolve_include(root)
                .map(|_| directive) // 如果 resolve_include 成功，返回 directive
                .map_err(|e| CustomError::ConfigError(format!("Include resolution error: {}", e)))
        })
        .collect::<Result<Vec<_>>>()?;

    let gateway_result = processed_directives.into_iter().find_map(|directive| {
        if directive.name == "pishoo" {
            directive.children.map(|children| {
                parse_gateway(children).map_err(|e| {
                    CustomError::ConfigError(format!("Failed to parse 'pishoo' block: {}", e))
                })
            })
        } else {
            None
        }
    });

    gateway_result
        .ok_or_else(|| CustomError::MissingConfig("pishoo".to_string()))
        .and_then(|inner_result| inner_result)
}
