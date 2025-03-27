//! Gateway configuration parser
//!
//! Handles parsing and management of server configurations including:
//! - Reverse proxy server clusters
//! - Forward proxy server instances
//! - Configuration validation and error handling

use std::{collections::HashMap, net::SocketAddr};

use misc_conf::{ast::Directive, nginx::Nginx};

use super::{proxy::ProxyConfig, server::ServerConfig};
use crate::error::{CustomError, Result};

#[derive(Debug, Default)]
pub struct Gateway {
    // TODO 新增 mime types 配置项, hashmap 类型, key 为文件后缀, value 为 mime 类型
    pub servers: HashMap<SocketAddr, Server>,
}

#[derive(Debug)]
pub enum Server {
    Reverse(Vec<ServerConfig>),
    Forward(ProxyConfig),
}

impl Gateway {
    pub fn insert_server(&mut self, server: ServerConfig) -> Result<()> {
        let addr = server.listen;
        self.servers
            .entry(addr)
            .and_modify(|record| {
                if let Server::Reverse(servers) = record {
                    servers.push(server.clone());
                }
            })
            .or_insert_with(|| Server::Reverse(vec![server]));
        Ok(())
    }

    pub fn insert_proxy(&mut self, proxy: ProxyConfig) -> Result<()> {
        let addr = proxy.listen;
        if self.servers.contains_key(&addr) {
            return Err(CustomError::DuplicateServer(addr));
        }
        self.servers.insert(addr, Server::Forward(proxy));
        Ok(())
    }
}

pub fn parse_gateway(directives: Vec<Directive<Nginx>>) -> Result<Gateway> {
    let mut gateway = Gateway::default();

    for directive in directives {
        match directive.name.as_str() {
            "allow" | "deny" => {}
            "server" => {
                if let Some(directives) = directive.children {
                    gateway.insert_server(ServerConfig::parse(directives)?)?;
                }
            }
            "proxy" => {
                if let Some(directives) = directive.children {
                    gateway.insert_proxy(ProxyConfig::parse(directives)?)?;
                }
            }
            unknown => return Err(CustomError::UnknownDirective(unknown.into())),
        }
    }

    Ok(gateway)
}
