use std::{collections::HashMap, net::SocketAddr, path::PathBuf, str::FromStr};

use http::{HeaderName, HeaderValue, Uri};
use misc_conf::{ast::Directive, nginx::Nginx};
use snafu::{OptionExt, ResultExt, ensure_whatever, whatever};

use super::{
    Result, Value,
    types::{IfaceRange, IpFamilies, Listens, ServerName},
};
use crate::error::Whatever;

#[allow(dead_code)]
pub(crate) fn parse_string_map(directive: Directive<Nginx>) -> Result<Value> {
    if let Some(children) = directive.children {
        let mut map = HashMap::new();
        for directive in children {
            let value = directive.name;
            for arg in directive.args {
                map.insert(arg, value.clone());
            }
        }
        return Ok(Value::StringMap(map));
    }
    Ok(Value::ValueMap(HashMap::new()))
}

pub(crate) fn parse_boolean(directive: Directive<Nginx>) -> Result<Value> {
    match &directive.args[..] {
        [flag] => match flag.as_str() {
            "on" => Ok(Value::Boolean(true)),
            "off" => Ok(Value::Boolean(false)),
            _ => whatever!("invalid boolean value `{flag}`, expected `on` or `off`"),
        },
        _ => whatever!(
            "invalid number of arguments for directive: {}",
            directive.name
        ),
    }
}

pub(super) fn parse_header_value(directive: Directive<Nginx>) -> Result<Value> {
    match &directive.args[..] {
        [value] => {
            let header_value = HeaderValue::from_str(value)
                .whatever_context(format!("failed to parse `{value}` to header value"))?;
            Ok(Value::HeaderValue(header_value))
        }
        _ => whatever!(
            "invalid number of arguments for directive: {}",
            directive.name
        ),
    }
}

pub(super) fn parse_types(directive: Directive<Nginx>) -> Result<Value> {
    if let Some(children) = directive.children {
        let mut map = HashMap::new();
        for directive in children {
            let value = directive.name.as_str();
            let value = HeaderValue::from_str(value)
                .whatever_context(format!("failed to parse `{value}` to header value"))?;
            for arg in directive.args {
                map.insert(arg, value.clone());
            }
        }
        return Ok(Value::Types(map));
    }
    Ok(Value::ValueMap(HashMap::new()))
}

pub(crate) fn parse_string(directive: Directive<Nginx>) -> Result<Value> {
    match &directive.args[..] {
        [string] => Ok(Value::String(string.to_string())),
        _ => whatever!(
            "invalid number of arguments for directive: {}",
            directive.name
        ),
    }
}

pub(super) fn parse_proxy_pass(directive: Directive<Nginx>) -> Result<Value> {
    match &directive.args[..] {
        [s] => {
            let uri = s.parse::<Uri>().whatever_context(format!(
                "invalid uri `{s}` while parsing directive {}",
                directive.name
            ))?;

            let scheme = uri
                .scheme_str()
                .whatever_context::<_, Whatever>("missing scheme in proxy_pass uri")
                .whatever_context(format!(
                    "invalid uri `{s}` while parsing directive {}",
                    directive.name
                ))?;

            ensure_whatever!(
                matches!(scheme, "http" | "https"),
                "invalid proxy_pass scheme `{scheme}`, expected `http` or `https`"
            );

            uri.host()
                .whatever_context::<_, Whatever>("missing host in proxy_pass uri")
                .whatever_context(format!(
                    "invalid uri `{s}` while parsing directive {}",
                    directive.name
                ))?;

            Ok(Value::Uri(uri))
        }
        _ => whatever!(
            "invalid number of arguments for directive: {}",
            directive.name
        ),
    }
}

/// listen [all/external/internal/lo/en0] [v6only|v4only|dual] [0|80]
pub(super) fn parse_listen(directive: Directive<Nginx>) -> Result<Value> {
    match &directive.args[..] {
        [iface] => {
            // Check if iface is actually a list of addresses
            if iface.contains(',') || iface.parse::<SocketAddr>().is_ok() {
                let addrs = iface
                    .split(',')
                    .map(|s| {
                        s.trim().parse::<SocketAddr>().whatever_context(format!(
                            "invalid socket address `{s}` while parsing directive {}",
                            directive.name
                        ))
                    })
                    .collect::<Result<Vec<SocketAddr>, Whatever>>();

                if let Ok(addrs) = addrs {
                    return Ok(Value::Listen(vec![Listens {
                        range: IfaceRange::All,
                        families: IpFamilies::Dual,
                        port: 0,
                        specific_addrs: Some(addrs),
                    }]));
                }
            }

            // 单个参数 只能是网卡名, 省略了 families 和端口的情况
            Ok(Value::Listen(vec![Listens {
                range: IfaceRange::from(iface.as_str()),
                families: IpFamilies::default(),
                port: 0,
                specific_addrs: None,
            }]))
        }
        [iface, param] => {
            // 两个参数, 可能是
            // 1. 网卡名和 v6only|v4only|dual
            // 2. 网卡名和端口

            let range = IfaceRange::from(iface.as_str());
            match IpFamilies::from_str(param) {
                Ok(families) => Ok(Value::Listen(vec![Listens {
                    range,
                    families,
                    port: 0,
                    specific_addrs: None,
                }])),
                Err(_) => {
                    let port = param.parse::<u16>().whatever_context(format!(
                        "invalid argument for directive: {}:{}",
                        directive.name, param
                    ))?;
                    Ok(Value::Listen(vec![Listens {
                        range,
                        families: IpFamilies::default(),
                        port,
                        specific_addrs: None,
                    }]))
                }
            }
        }
        [iface, version, port] => {
            // 三个参数, 只能是 网卡名和 v6only|v4only|dual 和端口
            let range = IfaceRange::from(iface.as_str());
            let families = IpFamilies::from_str(version.as_str())?;
            let port = port.parse::<u16>().whatever_context(format!(
                "invalid port number `{port}` while parsing directive {}",
                directive.name
            ))?;
            Ok(Value::Listen(vec![Listens {
                range,
                families,
                port,
                specific_addrs: None,
            }]))
        }
        _ => whatever!(
            "invalid number of arguments for directive: {}",
            directive.name
        ),
    }
}

pub(crate) fn parse_address(directive: Directive<Nginx>) -> Result<Value> {
    match &directive.args[..] {
        [string] => {
            if string.contains(',') {
                let addrs = string
                    .split(',')
                    .map(|s| {
                        s.trim().parse::<SocketAddr>().whatever_context(format!(
                            "invalid socket address `{s}` while parsing directive {}",
                            directive.name
                        ))
                    })
                    .collect::<Result<Vec<SocketAddr>, Whatever>>()?;
                Ok(Value::AddrVec(addrs))
            } else {
                let addr = string.parse::<SocketAddr>().whatever_context(format!(
                    "invalid socket address `{string}` while parsing directive {}",
                    directive.name
                ))?;
                Ok(Value::Addr(addr))
            }
        }
        _ => whatever!(
            "invalid number of arguments for directive: {}",
            directive.name
        ),
    }
}

pub(super) fn parse_resolver(directive: Directive<Nginx>) -> Result<Value> {
    match &directive.args[..] {
        [kind, resolver] => match kind.as_str() {
            "udp" => {
                whatever!("`udp` resolver is deprecated, please use `h3` instead",)
            }
            "http" => {
                whatever!("`http` resolver is deprecated, please use `h3` instead",)
            }
            "h3" => {
                let base_url = resolver.parse::<Uri>().whatever_context(format!(
                    "invalid base url `{resolver}` whiling parsing h3 resolver",
                ))?;

                Ok(Value::Resolver(base_url))
            }
            _ => whatever!("unknown resolver kind: {kind}, expected `h3`"),
        },
        _ => whatever!(
            "invalid number of arguments for directive: {}",
            directive.name
        ),
    }
}

pub(super) fn parse_path(directive: Directive<Nginx>) -> Result<Value> {
    match &directive.args[..] {
        [string] => {
            let path = PathBuf::from(string);
            ensure_whatever!(path.exists(), "path `{}` does not exist", path.display());
            Ok(Value::Path(path))
        }
        _ => whatever!(
            "invalid number of arguments for directive: {}",
            directive.name
        ),
    }
}

pub(crate) fn parse_string_vec(directive: Directive<Nginx>) -> Result<Value> {
    Ok(Value::StringVec(directive.args))
}

pub(crate) fn parse_server_name(directive: Directive<Nginx>) -> Result<Value> {
    let names: Vec<ServerName> = directive
        .args
        .into_iter()
        .map(|name| ServerName { name })
        .collect();
    Ok(Value::ServerName(names))
}

pub(crate) fn parse_server_id(directive: Directive<Nginx>) -> Result<Value> {
    match &directive.args[..] {
        [id_str] => {
            let id = id_str.parse::<u8>().whatever_context(format!(
                "invalid server id `{id_str}` while parsing directive {}",
                directive.name
            ))?;
            Ok(Value::ServerId(id))
        }
        _ => whatever!(
            "invalid number of arguments for directive: {}",
            directive.name
        ),
    }
}

pub(super) fn parse_header(directive: Directive<Nginx>) -> Result<Value> {
    match &directive.args[..] {
        [name, value] => {
            let header_name = HeaderName::from_bytes(name.as_bytes()).whatever_context(format!(
                "invalid header name `{name}` while parsing directive {}",
                directive.name
            ))?;
            let header_value =
                HeaderValue::from_bytes(value.as_bytes()).whatever_context(format!(
                    "invalid header value `{value}` while parsing directive {}",
                    directive.name
                ))?;
            Ok(Value::Header(vec![(header_name, header_value, true)]))
        }
        _ => whatever!(
            "invalid number of arguments for directive: {}",
            directive.name
        ),
    }
}

pub(super) fn parse_header_always(directive: Directive<Nginx>) -> Result<Value> {
    match &directive.args[..] {
        [name, value] => {
            let header_name = HeaderName::from_bytes(name.as_bytes()).whatever_context(format!(
                "invalid header name `{name}` while parsing directive {}",
                directive.name
            ))?;
            let header_value =
                HeaderValue::from_bytes(value.as_bytes()).whatever_context(format!(
                    "invalid header value `{value}` while parsing directive {}",
                    directive.name
                ))?;
            Ok(Value::Header(vec![(header_name, header_value, false)]))
        }
        [name, value, always] => {
            ensure_whatever!(
                always == "always",
                "the third argument of directive {} must be `always`",
                directive.name
            );
            let header_name = HeaderName::from_bytes(name.as_bytes()).whatever_context(format!(
                "invalid header name `{name}` while parsing directive {}",
                directive.name
            ))?;
            let header_value =
                HeaderValue::from_bytes(value.as_bytes()).whatever_context(format!(
                    "invalid header value `{value}` while parsing directive {}",
                    directive.name
                ))?;
            Ok(Value::Header(vec![(header_name, header_value, true)]))
        }
        _ => whatever!(
            "invalid number of arguments for directive: {}",
            directive.name
        ),
    }
}

pub(super) fn parse_ssh_login(directive: Directive<Nginx>) -> Result<Value> {
    let auths = directive
        .args
        .iter()
        .map(|auth| {
            ensure_whatever!(
                auth == "ssl",
                "invalid value for directive: {}",
                directive.name
            );
            Ok(auth.to_string())
        })
        .collect::<Result<Vec<_>>>()?;
    if auths.is_empty() {
        whatever!(
            "at least one authentication method is required for directive: {}",
            directive.name
        );
    }
    Ok(Value::StringVec(auths))
}

pub(super) fn parse_ssh_ssl_user(directive: Directive<Nginx>) -> Result<Value> {
    match &directive.args[..] {
        [name, user] => Ok(Value::SshSslUser(vec![(
            name.to_string(),
            user.to_string(),
        )])),
        _ => whatever!(
            "invalid number of arguments for directive: {}",
            directive.name
        ),
    }
}
