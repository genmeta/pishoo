use misc_conf::{ast::Directive, nginx::Nginx};

use crate::parse::{
    Commands, Result, Value, parse_boolean, parse_header_value, parse_string, parse_string_vec,
    parse_types, proxy::parse_proxy, server::parse_server,
};

pub(super) fn parse_pishoo(directive: Directive<Nginx>) -> Result<Value> {
    let mut commands = Commands::new();

    commands.insert("pid", parse_string);
    commands.insert("gzip", parse_boolean);
    commands.insert("gzip_vary", parse_boolean);
    commands.insert("gzip_min_length", parse_string);
    commands.insert("gzip_comp_level", parse_string);
    commands.insert("gzip_types", parse_string_vec);
    commands.insert("types", parse_types);
    commands.insert("access_rules", parse_string);
    commands.insert("default_type", parse_header_value);
    commands.insert("workers", parse_string_vec);
    commands.insert("groups", parse_string_vec);
    commands.insert("server", parse_server);
    commands.insert("proxy", parse_proxy);

    let values = commands.parse(directive.children.into_iter().flatten())?;

    Ok(Value::ValueMap(values))
}
