use std::path::Path;

use gateway::{Gateway, parse_gateway};
use misc_conf::{
    ast::{Directive, DirectiveTrait},
    nginx::Nginx,
};
use tracing::error;

use crate::error::{CustomError, Result};

pub mod gateway;
pub mod location;
pub mod pattern;
pub mod proxy;
pub mod router;
pub mod rule;
pub mod server;

pub fn parse_conf(configure: &[u8], root: &Path) -> Result<Gateway> {
    let mut gateway = Gateway::default();
    if let Ok(res) = Directive::<Nginx>::parse(configure) {
        for mut directive in res {
            directive
                .resolve_include(root)
                .map_err(|e| CustomError::UnknownDirective(format!("include error: {}", e)))?;
            if directive.name == "pishoo" {
                if let Some(children) = directive.children {
                    println!("children: {:#?}", children);
                    gateway =
                        parse_gateway(children).inspect_err(|e| error!("parse error: {}", e))?;
                    break;
                }
            }
        }
    }
    Ok(gateway)
}
