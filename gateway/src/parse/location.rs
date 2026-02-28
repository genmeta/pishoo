use http::{HeaderName, HeaderValue};
use misc_conf::{ast::Directive, nginx::Nginx};
use snafu::{ResultExt, whatever};

use crate::parse::{
    Commands, Result, Value, parse_address, parse_boolean, parse_header, parse_header_always,
    parse_path, parse_proxy_pass, parse_ssh_login, parse_ssh_ssl_user, parse_string,
    parse_string_vec, parse_types, pattern::parse_pattern,
};

pub(super) fn parse_location(directive: Directive<Nginx>) -> Result<Value> {
    let mut commands = Commands::new();

    commands.insert("types", parse_types);
    commands.insert("root", parse_path);
    commands.insert("alias", parse_path);
    commands.insert("gzip", parse_boolean);
    commands.insert("gzip_vary", parse_boolean);
    commands.insert("gzip_min_length", parse_string);
    commands.insert("gzip_comp_level", parse_string);
    commands.insert("gzip_types", parse_string_vec);
    commands.insert("index", parse_string_vec);
    commands.insert("add_header", parse_header_always);
    commands.insert("proxy_set_header", parse_header);
    commands.insert("proxy_pass", parse_proxy_pass);
    commands.insert("ssh_login", parse_ssh_login);
    commands.insert("ssh_ssl_user", parse_ssh_ssl_user);
    commands.insert("ssh_deny", parse_string_vec);
    // stun directives (used in `location /stun { ... }`)
    commands.insert("relay", parse_boolean);
    commands.insert("bind", parse_stun_bind);

    let pattern =
        parse_pattern(&directive.args).whatever_context("Failed to parse location pattern")?;
    let mut values = commands.parse(directive.children.into_iter().flatten())?;

    // 默认添加 CORS 相关的响应头
    let cors_headers = vec![
        // (
        //     HeaderName::from_static("access-control-allow-origin"),
        //     HeaderValue::from_static("tauri://localhost"),
        //     true,
        // ),
        // (
        //     HeaderName::from_static("access-control-allow-methods"),
        //     HeaderValue::from_static("*"),
        //     true,
        // ),
        (
            HeaderName::from_static("server"),
            HeaderValue::from_static("pishoo"),
            true,
        ),
    ];

    let Value::Header(exist_headers) = values
        .entry("add_header".to_string())
        .or_insert_with(|| Value::Header(vec![]))
    else {
        unreachable!("Unexpected value type, should be `Header`");
    };
    exist_headers.extend(cors_headers);

    Ok(Value::Pattern(pattern, values))
}

/// 解析 `bind <addr> { outer ...; change_addr ...; change_port ...; }` 块指令
///
/// `bind` 作为 `location /stun` 的子指令，可出现多次（多地址族 / 多绑定）。
/// 带子块时解析内部指令；无子块时仅记录绑定地址。
fn parse_stun_bind(directive: Directive<Nginx>) -> Result<Value> {
    let bind_addr = match &directive.args[..] {
        [addr_str] => addr_str
            .parse::<std::net::SocketAddr>()
            .map_err(|_| {
                snafu::FromString::without_source(format!(
                    "Invalid socket address `{addr_str}` for `bind`"
                ))
            })
            .map_err(|e: crate::error::Whatever| e)?,
        _ => whatever!(
            "Expected exactly one argument for `bind`, got {}",
            directive.args.len()
        ),
    };

    let mut inner = Commands::new();
    inner.insert("outer", parse_address);
    inner.insert("change_addr", parse_address);
    inner.insert("change_port", parse_string);
    let values = inner.parse(directive.children.into_iter().flatten())?;

    let mut map = std::collections::HashMap::new();
    map.insert("bind_address".to_string(), Value::Addr(bind_addr));
    map.extend(values);

    Ok(Value::ValueMap(map))
}
