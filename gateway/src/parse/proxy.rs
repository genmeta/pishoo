use anyhow::{Result, bail};
use misc_conf::{ast::Directive, nginx::Nginx};

use super::{
    Value, parse_address, parse_header_value, parse_resolver, parse_string_vec, parse_types,
};
use crate::parse::Commands;

pub(super) fn parse_proxy(directive: Directive<Nginx>) -> Result<Value> {
    let mut commands = Commands::new();

    commands.insert("listen", parse_address);
    commands.insert("resolver", parse_resolver);
    commands.insert("allow", parse_string_vec);
    commands.insert("deny", parse_string_vec);
    commands.insert("types", parse_types);
    commands.insert("default_type", parse_header_value);

    let values = commands.parse(directive.children.into_iter().flatten())?;

    if !values.contains_key("listen") {
        bail!("Missing directive listen")
    }
    Ok(Value::ValueMap(values))
}
