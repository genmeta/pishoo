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
    /// Index files
    pub index_files: Vec<String>,
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
            "index" => {
                gateway.index_files = directive.args;
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

    organize_mime_types(&mut gateway);
    organize_index_files(&mut gateway);

    Ok(gateway)
}

/// Helper function to mime types
fn organize_mime_types(gateway: &mut Gateway) {
    let mut parent_mime_types = &gateway.types;
    let mut parent_default_type = &gateway.default_type;
    // Process MIME types inheritance with override logic
    for server in gateway.servers.values_mut() {
        if let Server::Reverse(servers) = server {
            for server_config in servers {
                if !server_config.types.is_empty() {
                    parent_mime_types = &server_config.types;
                }
                if server_config.default_type.is_some() {
                    parent_default_type = &server_config.default_type;
                }

                for (_, location) in server_config.router.locations.iter_mut() {
                    match location {
                        Location::Root(file_location) | Location::Alias(file_location) => {
                            if file_location.mime_types.is_empty() {
                                file_location.mime_types = parent_mime_types.clone();
                            }
                            if file_location.default_type.is_none() {
                                file_location.default_type = parent_default_type.clone();
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

fn organize_index_files(gateway: &mut Gateway) {
    let mut parent_index_files = &gateway.index_files;
    // Process index files inheritance with override logic
    for server in gateway.servers.values_mut() {
        if let Server::Reverse(servers) = server {
            for server_config in servers {
                if !server_config.index_files.is_empty() {
                    parent_index_files = &server_config.index_files;
                }
                for (_, location) in server_config.router.locations.iter_mut() {
                    match location {
                        Location::Root(file_location) | Location::Alias(file_location) => {
                            if file_location.index_files.is_empty() {
                                file_location.index_files = parent_index_files.clone();
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}
