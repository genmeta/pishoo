//! Server configuration parser
//!
//! Handles parsing of server blocks and their components including:
//! - Listen directives
//! - SSL certificate configuration
//! - Access control lists

use std::{net::SocketAddr, path::Path};

use derive_builder::Builder;
use misc_conf::{ast::Directive, nginx::Nginx};

use super::{location::Location, router::Router};
use crate::error::{CustomError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerKind {
    Forward,
    Reverse,
}

#[derive(Builder, Debug, Clone)]
pub struct ServerConfig {
    pub kind: ServerKind,
    pub listen: SocketAddr,
    pub server_name: Vec<String>,
    #[builder(default = "false")]
    pub reuse_port: bool,
    pub cert: String,
    pub key: String,
    #[builder(default)]
    pub router: Router,
    #[builder(default = "Vec::new()")]
    pub allow: Vec<String>,
    #[builder(default = "Vec::new()")]
    pub deny: Vec<String>,
}

fn parse_path(directive: Directive<Nginx>) -> Result<String> {
    let [path]: &[String] = directive.args.as_ref() else {
        return Err(CustomError::InvalidArgs(directive.name));
    };

    if !Path::new(path).exists() {
        return Err(CustomError::FileNotFound(path.into()));
    }

    Ok(path.clone())
}

fn parse_listen(directive: Directive<Nginx>) -> Result<(SocketAddr, ServerKind)> {
    let addr: SocketAddr = directive
        .args
        .first()
        .ok_or_else(|| CustomError::MissingField("listen address".to_string()))
        .and_then(|addr| {
            addr.parse()
                .map_err(|e| CustomError::InvalidArgs(format!("{}: {}", addr, e)))
        })?;

    match &directive.args[1..] {
        [] => Ok((addr, ServerKind::Forward)),
        [ssl, version] if ssl == "ssl" && version == "http3" => Ok((addr, ServerKind::Reverse)),
        _ => Err(CustomError::InvalidArgs("listen".to_string())),
    }
}

impl ServerConfig {
    pub fn parse(directives: Vec<Directive<Nginx>>) -> Result<Self> {
        let mut builder = ServerConfigBuilder::default();
        for directive in directives {
            match directive.name.as_str() {
                "listen" => {
                    let (listen, kind) = parse_listen(directive)?;
                    builder.listen(listen).kind(kind)
                }
                "server_name" => builder.server_name(directive.args),
                "ssl_certificate" => builder.cert(parse_path(directive)?),
                "ssl_certificate_key" => builder.key(parse_path(directive)?),
                "allow" => builder.allow(directive.args),
                "deny" => builder.deny(directive.args),
                "reuse_port" => {
                    builder.reuse_port(directive.args.first().is_some_and(|on| on == "on"))
                }
                "location" => {
                    let mut router = builder.router.take().unwrap_or_default();
                    router.insert(Location::parse(directive)?);
                    builder.router(router)
                }
                _ => return Err(CustomError::UnknownDirective(directive.name)),
            };
        }

        // 验证 SSL 证书配置
        if let Some(ServerKind::Forward) = builder.kind.as_ref() {
            if builder.cert.is_none() {
                return Err(CustomError::MissingField("ssl_certificate".to_string()));
            }
            if builder.key.is_none() {
                return Err(CustomError::MissingField("ssl_certificate_key".to_string()));
            }
        }

        builder
            .build()
            .map_err(|e| CustomError::MissingField(e.to_string()))
    }
}
