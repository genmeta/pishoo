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

#[derive(Builder, Debug, Clone)]
pub struct ServerConfig {
    pub listen: SocketAddr,
    #[builder(default = "Vec::new()")]
    pub server_name: Vec<String>,
    #[builder(default = "false")]
    pub reuse_port: bool,
    pub resolver: SocketAddr,
    #[builder(default)]
    pub cert: String,
    #[builder(default)]
    pub key: String,
    #[builder(default)]
    pub router: Router,
    // TODO allow deny 变更为 ACL 列表,
    // 首先检测是否有黑名单, 如果当前域名在黑名单中则拒绝访问
    // 然后检测是否有白名单, 如果当前域名在白名单中则允许访问
    // 应该不允许同时启动黑白名单
    #[builder(default = "Vec::new()")]
    pub allow: Vec<String>,
    #[builder(default = "Vec::new()")]
    pub deny: Vec<String>,
    // TODO 新增 mime types 配置项, hashmap 类型, key 为文件后缀, value 为 mime 类型
}

impl ServerConfig {
    /// Parse a file path directive and validate file existence
    fn parse_path(directive: &Directive<Nginx>) -> Result<String> {
        let path = directive
            .args
            .first()
            .ok_or_else(|| CustomError::InvalidArgs(directive.name.clone()))?;

        if !Path::new(path).exists() {
            return Err(CustomError::FileNotFound(path.clone()));
        }

        Ok(path.clone())
    }

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
        let mut builder = ServerConfigBuilder::default();
        let mut router = Router::default();

        for directive in directives {
            match directive.name.as_str() {
                "listen" => {
                    builder.listen(Self::parse_socket_addr(&directive, "listen")?);
                }
                "server_name" => {
                    builder.server_name(directive.args);
                }
                "resolver" => {
                    builder.resolver(Self::parse_socket_addr(&directive, "resolver")?);
                }
                "ssl_certificate" => {
                    builder.cert(Self::parse_path(&directive)?);
                }
                "ssl_certificate_key" => {
                    builder.key(Self::parse_path(&directive)?);
                }
                "allow" => {
                    builder.allow(directive.args);
                }
                "deny" => {
                    builder.deny(directive.args);
                }
                "reuse_port" => {
                    builder.reuse_port(directive.args.first().is_some_and(|on| on == "on"));
                }
                "location" => {
                    let (pattern, location) = Location::parse(directive)?;
                    router.insert(pattern, location);
                }
                _ => return Err(CustomError::UnknownDirective(directive.name)),
            }
        }

        // Set accumulated values
        builder.router(router);

        // 验证 SSL 证书配置
        if builder.cert.is_none() || builder.cert.as_ref().unwrap().is_empty() {
            return Err(CustomError::MissingField("ssl_certificate".to_string()));
        }

        if builder.key.is_none() || builder.key.as_ref().unwrap().is_empty() {
            return Err(CustomError::MissingField("ssl_certificate_key".to_string()));
        }

        if builder.resolver.is_none() {
            return Err(CustomError::MissingField("resolver".to_string()));
        }

        // TODO 将 pishoo 的 mime types 配置项移植到这里

        builder
            .build()
            .map_err(|e| CustomError::MissingField(e.to_string()))
    }
}
