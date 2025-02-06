use std::{collections::HashMap, net::SocketAddr};

use misc_conf::{ast::Directive, nginx::Nginx};

use super::server::{ReverseConfig, ForwardConfig, Server, parse_server};
use crate::error::{CustomError, Result};

#[derive(Debug, Default)]
pub struct Gateway {
    pub records: HashMap<SocketAddr, Record>,
}

#[derive(Debug)]
pub enum Record {
    Reverse(Vec<ReverseConfig>),
    Forward(ForwardConfig),
}

impl Gateway {
    pub fn insert(&mut self, server: Server) -> Result<()> {
        match server {
            Server::Reverse(fwd) => self.update_record(fwd.addr, |existing| match existing {
                Record::Reverse(v) => {
                    v.push(fwd);
                    Ok(())
                }
                Record::Forward(_) => Err(CustomError::DuplicateServer(fwd.addr)),
            }),
            Server::Forward(rev) => self.insert_unique(rev.addr, Record::Forward(rev)),
        }
    }

    fn update_record<F>(&mut self, addr: SocketAddr, action: F) -> Result<()>
    where
        F: FnOnce(&mut Record) -> Result<()>,
    {
        match self.records.entry(addr) {
            std::collections::hash_map::Entry::Occupied(mut e) => action(e.get_mut()),
            std::collections::hash_map::Entry::Vacant(e) => {
                let mut new = Record::Reverse(Vec::new());
                action(&mut new)?;
                e.insert(new);
                Ok(())
            }
        }
    }

    fn insert_unique(&mut self, addr: SocketAddr, record: Record) -> Result<()> {
        match self.records.entry(addr) {
            std::collections::hash_map::Entry::Occupied(_) => {
                Err(CustomError::DuplicateServer(addr))
            }
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(record);
                Ok(())
            }
        }
    }
}

pub fn parse_gateway(directives: Vec<Directive<Nginx>>) -> Result<Gateway> {
    let mut gateway = Gateway::default();

    for directive in directives {
        match directive.name.as_str() {
            "allow" => {}
            "deny" => {}
            "server" => {
                if let Some(children) = directive.children {
                    gateway.insert(parse_server(children)?)?;
                }
            }
            unknown => return Err(CustomError::UnknownDirective(unknown.into())),
        }
    }

    Ok(gateway)
}
