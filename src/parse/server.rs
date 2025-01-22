use std::{net::SocketAddr, str::FromStr};

use misc_conf::{ast::Directive, nginx::Nginx};
use tracing::info;

use super::{
    location::parse_location,
    router::Router,
    version::{ServerType, parse_server_type},
};
use crate::error::{CustomError, Result};

#[derive(Debug, Clone)]
pub enum Server {
    Forward(ForwardServer),
    Reverse(ReverseServer),
}

#[derive(Debug, Clone)]
pub struct ForwardServer {
    pub addr: SocketAddr,
    pub server_name: Vec<String>,
    pub reuse_port: bool,
    pub ssl_config: SslConfig,
    pub router: Router,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ReverseServer {
    pub addr: SocketAddr,
    pub router: Router,
}

#[derive(Debug, Clone)]
pub struct SslConfig {
    pub cert_path: String,
    pub key_path: String,
}

pub fn parse_server(children: Vec<Directive<Nginx>>) -> Result<Server> {
    let mut config = ServerConfig::default();
    let mut locations = Vec::new();

    for child in children {
        match child.name.as_str() {
            "listen" => (config.addr, config.is_ssl, config.typ) = parse_listen(child)?,
            "server_name" => {
                config.server_name = child.args.into_iter().map(|s| s.to_string()).collect()
            }
            "ssl_certificate" => config.cert_path = child.args.first().map(|s| s.to_string()),
            "ssl_certificate_key" => config.key_path = child.args.first().map(|s| s.to_string()),
            "allow" => config.allow = child.args.into_iter().map(|s| s.to_string()).collect(),
            "deny" => config.deny = child.args.into_iter().map(|s| s.to_string()).collect(),
            "reuse_port" => config.reuse_port = child.args.first().is_some_and(|arg| arg == "on"),
            "location" => locations.push(child),
            _ => {
                info!("unknown directive: {}", child.name);
                return Err(CustomError::UnknownDirective(child.name));
            }
        }
    }

    for location in locations {
        config
            .router
            .insert(parse_location(location, config.typ)?)?;
    }

    validate_config(&config)?;
    build_server(config)
}

#[derive(Default)]
struct ServerConfig {
    typ: ServerType,
    addr: Option<SocketAddr>,
    server_name: Vec<String>,
    is_ssl: bool,
    cert_path: Option<String>,
    key_path: Option<String>,
    reuse_port: bool,
    router: Router,
    allow: Vec<String>,
    deny: Vec<String>,
}

fn validate_config(config: &ServerConfig) -> Result<()> {
    let addr = config
        .addr
        .as_ref()
        .ok_or_else(|| CustomError::MissingConfig("addr".to_string()))?;

    if config.typ == ServerType::Forward {
        if config.server_name.is_empty() {
            return Err(CustomError::MissingConfig(format!(
                "{addr}.{}",
                "server_name"
            )));
        }

        if !config.is_ssl {
            return Err(CustomError::MissingConfig(format!("{addr}.{}", "ssl")));
        }

        validate_ssl_config(addr, &config.cert_path, &config.key_path)?;
    }

    Ok(())
}

fn validate_ssl_config(
    addr: &SocketAddr,
    cert_path: &Option<String>,
    key_path: &Option<String>,
) -> Result<()> {
    if let Some(cert_path) = cert_path {
        if !std::path::Path::new(cert_path).exists() {
            return Err(CustomError::FileNotFound(format!(
                "{}.{}: {}",
                addr, "ssl_certificate", cert_path
            )));
        }
    } else {
        return Err(CustomError::MissingConfig(format!(
            "{}.{}",
            addr, "ssl_certificate"
        )));
    }

    if let Some(key_path) = key_path {
        if !std::path::Path::new(key_path).exists() {
            return Err(CustomError::FileNotFound(format!(
                "{}.{}: {}",
                addr, "ssl_certificate_key", key_path
            )));
        }
    } else {
        return Err(CustomError::MissingConfig(format!(
            "{}.{}",
            addr, "ssl_certificate_key"
        )));
    }

    Ok(())
}

fn build_server(config: ServerConfig) -> Result<Server> {
    let addr = config.addr.unwrap();

    match config.typ {
        ServerType::Reverse => {
            let reverse_server = ReverseServer {
                addr,
                router: config.router,
            };
            Ok(Server::Reverse(reverse_server))
        }
        ServerType::Forward => {
            let ssl_config = SslConfig {
                cert_path: config.cert_path.unwrap(),
                key_path: config.key_path.unwrap(),
            };

            let forward_server = ForwardServer {
                addr,
                server_name: config.server_name,
                reuse_port: config.reuse_port,
                ssl_config,
                router: config.router,
                allow: config.allow,
                deny: config.deny,
            };

            Ok(Server::Forward(forward_server))
        }
    }
}

pub fn parse_listen(listen: Directive<Nginx>) -> Result<(Option<SocketAddr>, bool, ServerType)> {
    let addr = listen
        .args
        .first()
        .ok_or_else(|| CustomError::MissingConfig(format!("{}.{}", listen.name, "addr")))
        .and_then(|addr| SocketAddr::from_str(addr).map_err(CustomError::AddrParseError))?;

    let (is_ssl, server_type) = parse_listen_args(&listen.args[1..])?;

    Ok((Some(addr), is_ssl, server_type))
}

fn parse_listen_args(args: &[String]) -> Result<(bool, ServerType)> {
    let mut is_ssl = false;
    let mut server_type = ServerType::default();

    match args {
        [ssl, version] if ssl == "ssl" => {
            is_ssl = true;
            server_type = parse_server_type(version);
        }
        [version] => {
            server_type = parse_server_type(version);
            // HTTP3 必须与 SSL 一起使用
            if server_type == ServerType::Forward {
                return Err(CustomError::UnsupportedConfig(
                    "http3 must be used with ssl".to_string(),
                ));
            }
        }
        _ => {}
    }

    Ok((is_ssl, server_type))
}
