use std::{net::SocketAddr, str::FromStr};

use misc_conf::{ast::Directive, nginx::Nginx};
use regex::Regex;
use tracing::info;

use crate::error::{CustomError, Result};

use super::{
    location::parse_location,
    pattern::Pattern,
    router::Router,
    rule::Rule,
    version::{HttpVersion, parse_http_version},
};

#[derive(Debug, Clone)]
pub struct Server {
    pub ssl_config: Option<SslConfig>,
    pub router: Router,
}

#[derive(Debug, Clone)]
pub struct SslConfig {
    pub cert_path: String,
    pub key_path: String,
}

impl Server {
    pub fn route(&self, path: &str) -> Result<(String, Vec<Rule>)> {
        let mut pattern = String::new();
        let mut rules = Vec::new();

        for location in self.router.locations() {
            match &location.pattern {
                Pattern::Exact(p) => {
                    if path == p {
                        pattern = p.to_string();
                        rules = location.rules.clone();
                        break;
                    }
                }
                Pattern::Prefix(p) => {
                    // TODO 提前检测正则是否合法

                    let p = format!(r"^{}", p);
                    let re = Regex::new(&p)?;
                    if let Some(captures) = re.captures(path) {
                        // 获取匹配的部分
                        if let Some(matched) = captures.get(0) {
                            pattern = matched.as_str().to_string();
                        }
                        rules = location.rules.clone();
                        break;
                    }
                }
                Pattern::Regex(p) => {
                    // TODO 提前检测正则是否合法

                    let re = Regex::new(p)?;
                    if let Some(captures) = re.captures(path) {
                        // 获取匹配的部分
                        if let Some(matched) = captures.get(0) {
                            pattern = matched.as_str().to_string();
                        }
                        rules = location.rules.clone();
                        break;
                    }
                }
                Pattern::CRegex(p) => {
                    // TODO 提前检测正则是否合法

                    let p = format!(r"(?i){}", p);
                    let re = Regex::new(&p)?;
                    if let Some(captures) = re.captures(path) {
                        // 获取匹配的部分
                        if let Some(matched) = captures.get(0) {
                            pattern = matched.as_str().to_string();
                        }
                        rules = location.rules.clone();
                        break;
                    }
                }
                Pattern::NormalPrefix(p) => {
                    if path.starts_with(p) {
                        pattern = p.to_string();
                        rules = location.rules.clone();
                        break;
                    }
                }
                Pattern::Common => {
                    pattern = path.to_string();
                    rules = location.rules.clone();
                    break;
                }
            }
        }

        Ok((pattern, rules))
    }
}

pub fn parse_server(
    children: Vec<Directive<Nginx>>,
) -> Result<(SocketAddr, String, HttpVersion, Server)> {
    let mut addr = None;
    let mut server_name = None;
    let mut version = HttpVersion::HTTP1;
    let mut is_ssl = false;
    let mut cert_path = None;
    let mut key_path = None;
    let mut router = Router::new();

    for child in children {
        match child.name.as_str() {
            "listen" => (addr, is_ssl, version) = parse_listen(child)?,
            "ssl" => is_ssl = child.args.first().is_some_and(|arg| arg == "on"),
            "server_name" => server_name = child.args.first().map(|s| s.to_string()),
            "ssl_certificate" => cert_path = child.args.first().map(|s| s.to_string()),
            "ssl_certificate_key" => key_path = child.args.first().map(|s| s.to_string()),
            "location" => router.insert(parse_location(child)?)?,
            _ => {
                info!("unknown directive: {}", child.name);
                return Err(CustomError::UnknownDirective(child.name));
            }
        }
    }

    // 检测是否缺少 addr 配置
    let addr = addr.ok_or_else(|| CustomError::MissingConfig("addr".to_string()))?;

    // 检测是否缺少 ssl 配置
    let ssl_config = if is_ssl {
        match (cert_path, key_path) {
            (Some(cert_path), Some(key_path)) => Some(SslConfig {
                cert_path,
                key_path,
            }),
            (None, _) => {
                return Err(CustomError::MissingConfig(format!(
                    "{addr}.{}",
                    "ssl_certificate"
                )));
            }
            (_, None) => {
                return Err(CustomError::MissingConfig(format!(
                    "{addr}.{}",
                    "ssl_certificate_key"
                )));
            }
        }
    } else {
        None
    };

    // 检测是否缺少 server_name 配置
    // TODO 目前强制要求配置 server_name
    let server_name = server_name
        .ok_or_else(|| CustomError::MissingConfig(format!("{addr}.{}", "server_name")))?;

    let server = Server { ssl_config, router };

    Ok((addr, server_name, version, server))
}

pub fn parse_listen(listen: Directive<Nginx>) -> Result<(Option<SocketAddr>, bool, HttpVersion)> {
    let addr = listen
        .args
        .first()
        .ok_or_else(|| CustomError::MissingConfig(format!("{}.{}", listen.name, "addr")))
        .and_then(|addr| SocketAddr::from_str(addr).map_err(CustomError::AddrParseError))?;

    let (is_ssl, http_version) = parse_listen_args(&listen.args[1..])?;

    Ok((Some(addr), is_ssl, http_version))
}

fn parse_listen_args(args: &[String]) -> Result<(bool, HttpVersion)> {
    let mut is_ssl = false;
    let mut http_version = HttpVersion::default();

    match args {
        [ssl, version] if ssl == "ssl" => {
            is_ssl = true;
            http_version = parse_http_version(version)?;
        }
        [version] => {
            http_version = parse_http_version(version)?;
            // HTTP3 必须与 SSL 一起使用
            if http_version == HttpVersion::HTTP3 {
                return Err(CustomError::UnsupportedConfig(
                    "http3 must be used with ssl".to_string(),
                ));
            }
        }
        _ => {
            return Err(CustomError::UnsupportedConfig(
                "invalid listen args".to_string(),
            ));
        }
    }

    Ok((is_ssl, http_version))
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn test_router() -> Result<()> {
        // let mut server = Server;
        // matches.insert(Pattern::Common, Vec::new())?;
        // matches.insert(Pattern::Exact("/api/v1/user/login".to_string()), Vec::new())?;
        // matches.insert(Pattern::Prefix("/api/v1".to_string()), Vec::new())?;
        // matches.insert(Pattern::CRegex("/api/v1/.*".to_string()), Vec::new())?;
        // matches.insert(Pattern::Regex("/".to_string()), Vec::new())?;
        // matches.insert(Pattern::NormalPrefix("/api".to_string()), Vec::new())?;

        // println!("{:?}", matches);
        Ok(())
    }
}
