use anyhow::{Result, bail};
use misc_conf::{ast::Directive, nginx::Nginx};

use super::{
    Value, location::parse_location, parse_header_value, parse_listen, parse_path, parse_resolver,
    parse_string_vec, parse_types,
};
use crate::parse::Commands;

pub(super) fn parse_server(directive: Directive<Nginx>) -> Result<Value> {
    let mut commands = Commands::new();

    commands.insert("listen", parse_listen);
    commands.insert("server_name", parse_string_vec);
    commands.insert("resolver", parse_resolver);
    commands.insert("ssl_certificate", parse_path);
    commands.insert("ssl_certificate_key", parse_path);
    commands.insert("location", parse_location);
    commands.insert("types", parse_types);
    commands.insert("default_type", parse_header_value);

    let values = commands.parse(directive.children.into_iter().flatten())?;

    if !values.contains_key("listen") {
        bail!("server directive must have listen directive");
    }

    if !values.contains_key("ssl_certificate") {
        bail!("server directive must have ssl_certificate directive");
    }
    if !values.contains_key("ssl_certificate_key") {
        bail!("server directive must have ssl_certificate_key directive");
    }

    Ok(Value::ValueMap(values))
}
