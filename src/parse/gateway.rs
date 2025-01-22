use std::{
    collections::{HashMap, hash_map::Entry},
    net::SocketAddr,
};

use misc_conf::{ast::Directive, nginx::Nginx};
use tracing::info;

use super::server::{ForwardConfig, ReverseConfig, parse_server};
use crate::{
    error::{CustomError, Result},
    parse::server::Server,
};

#[derive(Debug)]
pub struct Gateway {
    pub records: HashMap<SocketAddr, Record>,
}

#[derive(Debug)]
pub enum Record {
    Forward(Vec<ForwardConfig>),
    Reverse(ReverseConfig),
}

impl Gateway {
    pub fn new() -> Gateway {
        Gateway {
            records: HashMap::new(),
        }
    }

    pub fn insert(&mut self, server: Server) -> Result<()> {
        match server {
            Server::Forward(forward) => self.insert_forward(forward),
            Server::Reverse(reverse) => self.insert_reverse(reverse),
        }
    }

    fn insert_forward(&mut self, forward: ForwardConfig) -> Result<()> {
        match self.records.entry(forward.addr) {
            Entry::Occupied(mut entry) => match entry.get_mut() {
                Record::Forward(servers) => {
                    servers.push(forward);
                }
                Record::Reverse(_) => {
                    return Err(CustomError::DuplicateServer(forward.addr));
                }
            },
            Entry::Vacant(entry) => {
                entry.insert(Record::Forward(vec![forward]));
            }
        }
        Ok(())
    }

    fn insert_reverse(&mut self, reverse: ReverseConfig) -> Result<()> {
        match self.records.entry(reverse.addr) {
            Entry::Occupied(_) => Err(CustomError::DuplicateServer(reverse.addr)),
            Entry::Vacant(entry) => {
                entry.insert(Record::Reverse(reverse));
                Ok(())
            }
        }
    }
}

pub fn parse_gateway(children: Vec<Directive<Nginx>>) -> Result<Gateway> {
    let mut gateway = Gateway::new();
    for child in children {
        match child.name.as_str() {
            "allow" => {}
            "deny" => {}
            "server" => {
                if let Some(children) = child.children {
                    gateway.insert(parse_server(children)?)?;
                }
            }
            _ => {
                info!("unknown directive: {}", child.name);
                return Err(CustomError::UnknownDirective(child.name));
            }
        }
    }
    Ok(gateway)
}
