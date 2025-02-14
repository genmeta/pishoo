use std::{collections::HashMap, net::SocketAddr};

use misc_conf::{ast::Directive, nginx::Nginx};

use super::server::{ServerConfig, ServerKind};
use crate::error::{CustomError, Result};

#[derive(Debug, Default)]
pub struct Gateway {
    pub records: HashMap<SocketAddr, Record>,
}

#[derive(Debug)]
pub enum Record {
    Reverse(Vec<ServerConfig>),
    Forward(ServerConfig),
}

impl Gateway {
    pub fn insert(&mut self, config: ServerConfig) -> Result<()> {
        let addr = config.listen;
        match config.kind {
            ServerKind::Reverse => {
                self.records
                    .entry(addr)
                    .and_modify(|record| {
                        if let Record::Reverse(servers) = record {
                            servers.push(config.clone());
                        }
                    })
                    .or_insert_with(|| Record::Reverse(vec![config]));
                Ok(())
            }
            ServerKind::Forward => {
                if self.records.contains_key(&addr) {
                    return Err(CustomError::DuplicateServer(addr));
                }
                self.records.insert(addr, Record::Forward(config));
                Ok(())
            }
        }
    }
}

pub fn parse_gateway(directives: Vec<Directive<Nginx>>) -> Result<Gateway> {
    let mut gateway = Gateway::default();

    for directive in directives {
        match directive.name.as_str() {
            "allow" | "deny" => {}
            "server" => {
                if let Some(directives) = directive.children {
                    gateway.insert(ServerConfig::parse(directives)?)?;
                }
            }
            unknown => return Err(CustomError::UnknownDirective(unknown.into())),
        }
    }

    Ok(gateway)
}
