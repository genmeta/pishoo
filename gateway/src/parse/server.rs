use dhttp_home::identity::ssl::{CERT_FILE_NAME, KEY_FILE_NAME};
use misc_conf::{ast::Directive, nginx::Nginx};
use snafu::ensure_whatever;

use super::{
    Commands, IDENTITY_HOME, ServerName, Value,
    directives::{
        parse_address, parse_boolean, parse_header_value, parse_listen, parse_path, parse_resolver,
        parse_server_id, parse_server_name, parse_string, parse_string_vec, parse_types,
    },
    location::parse_location,
};
use crate::error::{Result, Whatever};

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

    // For worker identity configs (IDENTITY_HOME is set), auto-derive
    // server_name / ssl_certificate / ssl_certificate_key from the identity home.
    // Root pishoo.conf server blocks must configure these explicitly.
    if let Some(identity_home) = IDENTITY_HOME.with(|r| r.borrow().clone()) {
        if !values.contains_key("server_name") {
            let server_name = identity_home.name().as_full().to_string();
            values.insert(
                "server_name".to_string(),
                Value::ServerName(vec![ServerName { name: server_name }]),
            );
        }

        if !values.contains_key("ssl_certificate") {
            let cert_path = identity_home.ssl_dir().join(CERT_FILE_NAME);
            ensure_whatever!(
                cert_path.exists(),
                "ssl_certificate not configured and default cert `{}` does not exist",
                cert_path.display()
            );
            values.insert("ssl_certificate".to_string(), Value::Path(cert_path));
        }

        if !values.contains_key("ssl_certificate_key") {
            let key_path = identity_home.ssl_dir().join(KEY_FILE_NAME);
            ensure_whatever!(
                key_path.exists(),
                "ssl_certificate_key not configured and default key `{}` does not exist",
                key_path.display()
            );
            values.insert("ssl_certificate_key".to_string(), Value::Path(key_path));
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
