use std::collections::HashMap;

use anyhow::{Result, anyhow};
use http::{HeaderName, HeaderValue};
use misc_conf::{ast::Directive, nginx::Nginx};

use super::{
    ParseFn, Value, parse_header, parse_header_always, parse_path, parse_ssh_login,
    parse_ssh_ssl_user, parse_string_vec, parse_types, pattern::parse_pattern,
};
use crate::parse::parse_uri;

pub(super) fn parse_location(directive: Directive<Nginx>) -> Result<Value> {
    let mut commands: HashMap<&'static str, ParseFn> = HashMap::new();

    commands.insert("types", Box::new(parse_types));
    commands.insert("root", Box::new(parse_path));
    commands.insert("alias", Box::new(parse_path));
    commands.insert("index", Box::new(parse_string_vec));
    commands.insert("add_header", Box::new(parse_header_always));
    commands.insert("proxy_set_header", Box::new(parse_header));
    commands.insert("proxy_pass", Box::new(parse_uri));
    commands.insert("ssh_login", Box::new(parse_ssh_login));
    commands.insert("ssh_ssl_user", Box::new(parse_ssh_ssl_user));
    commands.insert("ssh_deny", Box::new(parse_string_vec));

    let pattern = parse_pattern(&directive.args)?;
    let mut values = HashMap::new();
    if let Some(children) = directive.children {
        for directive in children {
            let name = directive.name.clone();
            if let Some(command) = commands.get(name.as_str()) {
                match command(directive)? {
                    Value::Header(header) => {
                        if let Some(exist_value) = values.get_mut(&name) {
                            if let Value::Header(exist_header) = exist_value {
                                exist_header.extend(header);
                            } else {
                                return Err(anyhow!("unexpected value type"));
                            }
                        } else {
                            values.insert(name, Value::Header(header));
                        }
                    }
                    Value::SshSslUser(ssl_user) => {
                        if let Some(exist_value) = values.get_mut(&name) {
                            if let Value::SshSslUser(exist_header) = exist_value {
                                exist_header.extend(ssl_user);
                            } else {
                                return Err(anyhow!("unexpected value type"));
                            }
                        } else {
                            values.insert(name, Value::SshSslUser(ssl_user));
                        }
                    }
                    value => {
                        values.insert(name, value);
                    }
                }
            } else {
                return Err(anyhow!("unknown directive {}", name));
            }
        }
    }

    // 默认添加 CORS 相关的响应头
    let mut cors_header = vec![
        (
            HeaderName::from_static("access-control-allow-origin"),
            HeaderValue::from_static("tauri://localhost"),
            true,
        ),
        (
            HeaderName::from_static("access-control-allow-methods"),
            HeaderValue::from_static("*"),
            true,
        ),
        (
            HeaderName::from_static("server"),
            HeaderValue::from_static("pishoo"),
            true,
        ),
    ];

    values
        .entry("add_header".to_string())
        .and_modify(|value| {
            if let Value::Header(exist_header) = value {
                cors_header.extend_from_slice(exist_header);
                *exist_header = cors_header.clone();
            }
        })
        .or_insert_with(|| Value::Header(cors_header));

    Ok(Value::Pattern(pattern, values))
}
