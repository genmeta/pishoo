//! Gateway configuration parser
//!
//! Handles parsing and management of server configurations including:
//! - Reverse proxy server clusters
//! - Forward proxy server instances
//! - Configuration validation and error handling

use std::{collections::HashMap, net::SocketAddr};

use misc_conf::{ast::Directive, nginx::Nginx};

use super::{proxy::ProxyConfig, server::ServerConfig};
use crate::{
    error::{CustomError, Result},
    parse::location::Location,
};

#[derive(Debug, Default)]
pub struct Gateway {
    /// MIME types configuration
    pub types: HashMap<String, String>,
    /// Default MIME type
    pub default_type: Option<String>,
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
            "types" => {
                if let Some(directives) = directive.children {
                    for directive in directives {
                        for arg in directive.args {
                            gateway.types.insert(arg, directive.name.clone());
                        }
                    }
                }
            }
            "default_type" => {
                if let Some(arg) = directive.args.first() {
                    gateway.default_type = Some(arg.clone());
                }
            }
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

    // 继承 gateway 和 server 层级的 MIME types 配置到 location
    for server in gateway.servers.values_mut() {
        let mime_type = gateway.types.clone();
        let default_type = gateway.default_type.clone();
        if let Server::Reverse(servers) = server {
            for server in servers {
                let mut mime_type = mime_type.clone();
                let mut default_type = default_type.clone();

                for (key, value) in server.types.clone() {
                    mime_type.insert(key, value);
                }

                if let Some(value) = server.default_type.clone() {
                    default_type = Some(value);
                }

                for (_, location) in server.router.locations.iter_mut() {
                    match location {
                        Location::Root(file_location) | Location::Alias(file_location) => {
                            let mut mime_type = mime_type.clone();
                            let mut default_type = default_type.clone();
                            for (key, value) in file_location.mime_types.clone() {
                                mime_type.insert(key, value);
                            }

                            if let Some(value) = file_location.default_type.clone() {
                                default_type = Some(value);
                            }
                            file_location.mime_types = mime_type;
                            file_location.default_type = default_type;
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    Ok(gateway)
}
