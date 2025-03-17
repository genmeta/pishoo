//! Proxy configuration parser
//!
//! Handles parsing of server blocks and their components including:
//! - Listen directives
//! - Access control lists
//! - DNS server configuration

use std::net::SocketAddr;

use derive_builder::Builder;
use misc_conf::{ast::Directive, nginx::Nginx};

use crate::error::{CustomError, Result};

#[derive(Builder, Debug, Clone)]
pub struct ProxyConfig {
    pub listen: SocketAddr,
    pub resolver: SocketAddr,
    #[builder(default = "Vec::new()")]
    pub allow: Vec<String>,
    #[builder(default = "Vec::new()")]
    pub deny: Vec<String>,
}

impl ProxyConfig {
    /// Parse the first argument of a directive as a SocketAddr
    fn parse_socket_addr(directive: &Directive<Nginx>, field_name: &str) -> Result<SocketAddr> {
        directive
            .args
            .first()
            .ok_or_else(|| CustomError::MissingField(field_name.to_string()))
            .and_then(|addr| {
                addr.parse()
                    .map_err(|e| CustomError::InvalidArgs(format!("{}: {}", addr, e)))
            })
    }

    pub fn parse(directives: Vec<Directive<Nginx>>) -> Result<Self> {
        let mut builder = ProxyConfigBuilder::default();
        let mut allow_list = Vec::new();
        let mut deny_list = Vec::new();

        for directive in directives {
            match directive.name.as_str() {
                "listen" => {
                    builder.listen(Self::parse_socket_addr(&directive, "listen address")?);
                }
                "resolver" => {
                    builder.resolver(Self::parse_socket_addr(&directive, "resolver")?);
                }
                "allow" => allow_list.extend(directive.args),
                "deny" => deny_list.extend(directive.args),
                _ => return Err(CustomError::UnknownDirective(directive.name)),
            }
        }

        builder.allow(allow_list);
        builder.deny(deny_list);

        // Ensure dns_server is set
        if builder.resolver.is_none() {
            return Err(CustomError::MissingField("resolver".to_string()));
        }

        builder
            .build()
            .map_err(|e| CustomError::MissingField(e.to_string()))
    }
}
