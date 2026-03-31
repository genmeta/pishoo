use misc_conf::{ast::Directive, nginx::Nginx};
use snafu::ensure_whatever;

use crate::{
    error::{Result, Whatever},
    parse::{
        CONFIG_ROOT, Commands, ServerName, Value, location::parse_location, parse_address,
        parse_boolean, parse_header_value, parse_listen, parse_path, parse_resolver,
        parse_server_id, parse_server_name, parse_string, parse_string_vec, parse_types,
    },
};

pub(crate) fn parse_server(directive: Directive<Nginx>) -> Result<Value, Whatever> {
    let mut commands = Commands::new();

    commands.insert("listen", parse_listen);
    commands.insert("server_name", parse_server_name);
    commands.insert("server_id", parse_server_id);
    commands.insert("dns", parse_resolver);
    commands.insert("gzip", parse_boolean);
    commands.insert("gzip_vary", parse_boolean);
    commands.insert("gzip_min_length", parse_string);
    commands.insert("gzip_comp_level", parse_string);
    commands.insert("gzip_types", parse_string_vec);
    commands.insert("ssl_certificate", parse_path);
    commands.insert("ssl_certificate_key", parse_path);
    commands.insert("location", parse_location);
    commands.insert("types", parse_types);
    commands.insert("default_type", parse_header_value);
    commands.insert("access_rules", parse_string);
    commands.insert("relay", parse_boolean);
    commands.insert("stun", parse_boolean);
    commands.insert("stun_server", parse_stun_server);

    let mut values = commands.parse(directive.children.into_iter().flatten())?;

    ensure_whatever!(
        values.contains_key("listen"),
        "Server directive must have listen directive"
    );

    // 如果 server_name / ssl_certificate / ssl_certificate_key 未配置，
    // 则根据配置文件所在目录名自动推导
    if !values.contains_key("server_name")
        || !values.contains_key("ssl_certificate")
        || !values.contains_key("ssl_certificate_key")
    {
        let config_dir = CONFIG_ROOT.with(|r| r.borrow().clone());
        if let Some(config_dir) = config_dir {
            if !values.contains_key("server_name") {
                let dir_name = config_dir
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.to_string());
                ensure_whatever!(
                    dir_name.is_some(),
                    "server_name 未配置，且无法从配置目录 `{}` 推导域名",
                    config_dir.display()
                );
                let server_name = format!("{}.genmeta.net", dir_name.unwrap());
                values.insert(
                    "server_name".to_string(),
                    Value::ServerName(vec![ServerName { name: server_name }]),
                );
            }

            if !values.contains_key("ssl_certificate") {
                let cert_path = config_dir.join("fullchain.crt");
                ensure_whatever!(
                    cert_path.exists(),
                    "ssl_certificate 未配置，默认证书文件 `{}` 不存在",
                    cert_path.display()
                );
                values.insert("ssl_certificate".to_string(), Value::Path(cert_path));
            }

            if !values.contains_key("ssl_certificate_key") {
                let key_path = config_dir.join("privkey.pem");
                ensure_whatever!(
                    key_path.exists(),
                    "ssl_certificate_key 未配置，默认密钥文件 `{}` 不存在",
                    key_path.display()
                );
                values.insert("ssl_certificate_key".to_string(), Value::Path(key_path));
            }
        }
    }

    ensure_whatever!(
        values.contains_key("ssl_certificate"),
        "Server directive must have ssl_certificate directive"
    );

    ensure_whatever!(
        values.contains_key("ssl_certificate_key"),
        "Server directive must have ssl_certificate_key directive"
    );

    Ok(Value::ValueMap(values))
}

/// 解析 `stun_server { bind ...; outer_addr ...; change_addr ...; change_port ...; }` 块指令。
///
/// 出现在 `server` 块中，可重复出现（多地址族）。
/// 配置了该块代表初始（bootstrap）节点；无该块代表普通动态节点。
fn parse_stun_server(directive: Directive<Nginx>) -> Result<Value, Whatever> {
    let mut inner = Commands::new();
    inner.insert("bind", parse_address);
    inner.insert("outer_addr", parse_address);
    inner.insert("change_addr", parse_address);
    inner.insert("change_port", parse_string);
    let values = inner.parse(directive.children.into_iter().flatten())?;
    Ok(Value::ValueMap(values))
}
