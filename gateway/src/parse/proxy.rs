use misc_conf::{ast::Directive, nginx::Nginx};
use snafu::ensure_whatever;

use crate::parse::{
    Commands, Result, Value, parse_address, parse_header_value, parse_path, parse_resolver,
    parse_string, parse_string_vec, parse_types,
};

pub(super) fn parse_proxy(directive: Directive<Nginx>) -> Result<Value> {
    let mut commands = Commands::new();

    commands.insert("listen", parse_address);
    commands.insert("client_name", parse_string);
    commands.insert("resolver", parse_resolver);
    commands.insert("ssl_certificate", parse_path);
    commands.insert("ssl_certificate_key", parse_path);
    commands.insert("allow", parse_string_vec);
    commands.insert("deny", parse_string_vec);
    commands.insert("types", parse_types);
    commands.insert("default_type", parse_header_value);

    let values = commands.parse(directive.children.into_iter().flatten())?;

    ensure_whatever!(
        values.contains_key("listen"),
        "Missing directive listen in proxy block"
    );

    Ok(Value::ValueMap(values))
}
