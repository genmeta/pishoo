use std::collections::HashMap;

use anyhow::{Result, anyhow};
use misc_conf::{ast::Directive, nginx::Nginx};

use super::{ParseFn, Value, parse_address, parse_string, parse_string_map, parse_string_vec};

pub(super) fn parse_proxy(directive: Directive<Nginx>) -> Result<Value> {
    let mut sub_parser: HashMap<&'static str, ParseFn> = HashMap::new();

    sub_parser.insert("listen", Box::new(parse_address));
    sub_parser.insert("resolver", Box::new(parse_address));
    sub_parser.insert("allow", Box::new(parse_string_vec));
    sub_parser.insert("deny", Box::new(parse_string_vec));
    sub_parser.insert("types", Box::new(parse_string_map));
    sub_parser.insert("default_type", Box::new(parse_string));

    let mut values = HashMap::new();
    if let Some(children) = directive.children {
        for directive in children {
            let name = directive.name.clone();
            if let Some(parser) = sub_parser.get(name.as_str()) {
                values.insert(name, parser(directive)?);
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
