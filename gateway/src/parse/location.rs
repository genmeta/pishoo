use http::{HeaderName, HeaderValue};
use misc_conf::{ast::Directive, nginx::Nginx};
use snafu::ResultExt;

use crate::parse::{
    Commands, Result, Value, parse_boolean, parse_header, parse_header_always, parse_path,
    parse_proxy_pass, parse_ssh_login, parse_ssh_ssl_user, parse_string, parse_string_vec,
    parse_types, pattern::parse_pattern,
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
    commands.insert("access_log", parse_path);
    commands.insert("error_log", parse_path);
    commands.insert("ssh_login", parse_ssh_login);
    commands.insert("ssh_ssl_user", parse_ssh_ssl_user);
    commands.insert("ssh_deny", parse_string_vec);

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
