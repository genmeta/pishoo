use misc_conf::{ast::Directive, nginx::Nginx};
use snafu::ensure_whatever;

use crate::{
    error::{Result, Whatever},
    parse::{
        Commands, Value, location::parse_location, parse_boolean, parse_header_value, parse_listen,
        parse_path, parse_resolver, parse_server_id, parse_server_name, parse_string,
        parse_string_vec, parse_types,
    },
};

pub(super) fn parse_server(directive: Directive<Nginx>) -> Result<Value, Whatever> {
    let mut commands = Commands::new();

    commands.insert("listen", parse_listen);
    commands.insert("server_name", parse_server_name);
    commands.insert("server_id", parse_server_id);
    commands.insert("resolver", parse_resolver);
    commands.insert("stun_server", parse_boolean);
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

    let values = commands.parse(directive.children.into_iter().flatten())?;

    ensure_whatever!(
        values.contains_key("listen"),
        "Server directive must have listen directive"
    );

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
