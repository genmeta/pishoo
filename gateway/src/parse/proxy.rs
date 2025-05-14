use std::collections::HashMap;

use anyhow::{Result, anyhow};
use misc_conf::{ast::Directive, nginx::Nginx};

use super::{
    ParseFn, Value, parse_address, parse_header_value, parse_resolver, parse_string_vec,
    parse_types,
};

pub(super) fn parse_proxy(directive: Directive<Nginx>) -> Result<Value> {
    let mut commands: HashMap<&'static str, ParseFn> = HashMap::new();

    commands.insert("listen", Box::new(parse_address));
    commands.insert("resolver", Box::new(parse_resolver));
    commands.insert("allow", Box::new(parse_string_vec));
    commands.insert("deny", Box::new(parse_string_vec));
    commands.insert("types", Box::new(parse_types));
    commands.insert("default_type", Box::new(parse_header_value));

    let mut values = HashMap::new();
    if let Some(children) = directive.children {
        for directive in children {
            let name = directive.name.clone();
            if let Some(command) = commands.get(name.as_str()) {
                values.insert(name, command(directive)?);
            } else {
                return Err(anyhow!("unknown directive {}", name));
            }
        }
    }

    if !values.contains_key("listen") {
        return Err(anyhow!("missing directive listen"));
    }
    if !values.contains_key("resolver") {
        return Err(anyhow!("missing directive resolver"));
    }

    Ok(Value::ValueMap(values))
}
