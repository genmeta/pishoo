use std::{net::SocketAddr, path::Path};

use misc_conf::{ast::Directive, nginx::Nginx};

use super::{location::parse_location, router::Router};
use crate::error::{CustomError, Result};

// 统一配置类型
#[derive(Debug, Clone)]
pub enum Server {
    Forward(ForwardConfig),
    Reverse(ReverseConfig),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ServerType {
    #[default]
    Reverse,
    Forward,
}

#[derive(Debug, Clone)]
pub struct ForwardConfig {
    pub addr: SocketAddr,
    pub server_name: Vec<String>,
    pub reuse_port: bool,
    pub ssl: SslConfig,
    pub router: Router,
    pub access: AccessControl,
}

#[derive(Debug, Clone, Default)]
pub struct AccessControl {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ReverseConfig {
    pub addr: SocketAddr,
    pub router: Router,
}

#[derive(Debug, Clone, Default)]
pub struct SslConfig {
    pub cert: String,
    pub key: String,
}

#[derive(Default)]
struct ServerBuilder {
    typ: ServerType,
    addr: Option<SocketAddr>,
    server_name: Vec<String>,
    reuse_port: bool,
    ssl: Option<SslConfig>,
    router: Router,
    access: AccessControl,
}

impl ServerBuilder {
    fn parse_directive(&mut self, directive: Directive<Nginx>) -> Result<()> {
        match directive.name.as_str() {
            "listen" => self.parse_listen(directive)?,
            "server_name" => self.server_name = directive.args.into_iter().collect(),
            "ssl_certificate" => {
                self.ssl.get_or_insert_with(SslConfig::default).cert =
                    directive.args.first().cloned().unwrap_or_default()
            }
            "ssl_certificate_key" => {
                self.ssl.get_or_insert_with(SslConfig::default).key =
                    directive.args.first().cloned().unwrap_or_default()
            }
            "allow" => self.access.allow = directive.args,
            "deny" => self.access.deny = directive.args,
            "reuse_port" => {
                self.reuse_port = directive
                    .args
                    .first()
                    .map(|s| s == "on")
                    .unwrap_or_default()
            }
            "location" => self.router.insert(parse_location(directive, self.typ)?)?,
            _ => return Err(CustomError::UnknownDirective(directive.name)),
        }
        Ok(())
    }

    fn parse_listen(&mut self, directive: Directive<Nginx>) -> Result<()> {
        let addr_str = directive
            .args
            .first()
            .ok_or_else(|| CustomError::MissingField("listen address".to_string()))?;

        self.addr = Some(
            addr_str
                .parse()
                .map_err(|e| CustomError::InvalidArgs(format!("{}: {}", addr_str, e)))?,
        );

        let (is_ssl, typ) = match &directive.args[1..] {
            [] => (false, ServerType::Reverse),
            [ssl, version] if ssl == "ssl" && version == "http3" => (true, ServerType::Forward),
            _ => return Err(CustomError::InvalidArgs("listen".to_string())),
        };

        if is_ssl && self.ssl.is_none() {
            self.ssl = Some(SslConfig::default());
        }
        self.typ = typ;

        Ok(())
    }

    fn build(self) -> Result<Server> {
        let addr = self
            .addr
            .ok_or(CustomError::MissingField("address".to_string()))?;

        match self.typ {
            ServerType::Reverse => Ok(Server::Reverse(ReverseConfig {
                addr,
                router: self.router,
            })),
            ServerType::Forward => {
                let ssl = self
                    .ssl
                    .ok_or(CustomError::MissingField("SSL config".to_string()))?;
                ssl.validate()?;

                if self.server_name.is_empty() {
                    return Err(CustomError::MissingField("server_name".to_string()));
                }

                Ok(Server::Forward(ForwardConfig {
                    addr,
                    server_name: self.server_name,
                    reuse_port: self.reuse_port,
                    ssl,
                    router: self.router,
                    access: self.access,
                }))
            }
        }
    }
}

impl SslConfig {
    fn validate(&self) -> Result<()> {
        let check_file = |path: &str| {
            if !Path::new(path).exists() {
                Err(CustomError::FileNotFound(path.into()))
            } else {
                Ok(())
            }
        };

        check_file(&self.cert)?;
        check_file(&self.key)?;
        Ok(())
    }
}

pub fn parse_server(directives: Vec<Directive<Nginx>>) -> Result<Server> {
    let mut builder = ServerBuilder::default();

    for directive in directives {
        builder.parse_directive(directive)?;
    }

    builder.build()
}
