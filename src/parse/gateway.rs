use std::{collections::HashMap, net::SocketAddr};

use misc_conf::{ast::Directive, nginx::Nginx};
use tracing::info;

use super::server::parse_server;
use crate::{
    error::{CustomError, Result},
    parse::server::Server,
};

#[derive(Debug)]
pub struct Gateway {
    pub records: HashMap<SocketAddr, HashMap<String, Server>>,
}

impl Gateway {
    pub fn new() -> Gateway {
        Gateway {
            records: HashMap::new(),
        }
    }

    pub fn insert(&mut self, server: Server) {
        let record = self.records.entry(server.addr).or_default();
        record.insert(server.server_name.clone(), server);
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
