use anyhow::Result;
use misc_conf::{ast::Directive, nginx::Nginx};

use super::{Value, parse_header_value, parse_types, proxy::parse_proxy, server::parse_server};
use crate::parse::{Commands, parse_string};

pub(super) fn parse_pishoo(directive: Directive<Nginx>) -> Result<Value> {
    let mut commands = Commands::new();

    commands.insert("types", parse_types);
    commands.insert("access_rules", parse_string);
    commands.insert("default_type", parse_header_value);
    commands.insert("server", parse_server);
    commands.insert("proxy", parse_proxy);

    let values = commands.parse(directive.children.into_iter().flatten())?;

    Ok(Value::ValueMap(values))
}
