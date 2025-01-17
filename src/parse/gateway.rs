use misc_conf::{ast::Directive, nginx::Nginx};
use std::{collections::HashMap, net::SocketAddr};
use tracing::info;

use crate::error::{CustomError, Result};
use crate::parse::server::Server;

use super::server::parse_server;
use super::version::HttpVersion;

#[derive(Debug)]
pub struct Gateway {
    pub records: HashMap<SocketAddr, HashMap<HttpVersion, HashMap<String, Server>>>,
}

impl Gateway {
    pub fn new() -> Gateway {
        Gateway {
            records: HashMap::new(),
        }
    }

    pub fn insert(&mut self, server: Server) {
        let record = self.records.entry(server.addr).or_default();
        let servers = record.entry(server.version).or_default();
        servers.insert(server.server_name.clone(), server);
    }
}

pub fn parse_gateway(children: Vec<Directive<Nginx>>) -> Result<Gateway> {
    let mut gateway = Gateway::new();
    for child in children {
        match child.name.as_str() {
            "server" => {
                if let Some(children) = child.children {
                    let server = parse_server(children)?;
                    gateway.insert(server);
                }
            }
            "allow" => {}
            "deny" => {}
            _ => {
                info!("unknown directive: {}", child.name);
                return Err(CustomError::UnknownDirective(child.name));
            }
        }
    }
    Ok(gateway)
}
