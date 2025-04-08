use std::{collections::HashMap, sync::Arc};

use anyhow::{Result, anyhow};
use misc_conf::{ast::Directive, nginx::Nginx};

use super::{
    Node, ParseFn, Value, location::parse_location, parse_address, parse_path, parse_string,
    parse_string_map, parse_string_vec,
};

pub(super) fn parse_server(directive: Directive<Nginx>) -> Result<Value> {
    let mut commands: HashMap<&'static str, ParseFn> = HashMap::new();

    commands.insert("listen", Box::new(parse_address));
    commands.insert("server_name", Box::new(parse_string_vec));
    commands.insert("resolver", Box::new(parse_address));
    commands.insert("ssl_certificate", Box::new(parse_path));
    commands.insert("ssl_certificate_key", Box::new(parse_path));
    commands.insert("location", Box::new(parse_location));
    commands.insert("types", Box::new(parse_string_map));
    commands.insert("default_type", Box::new(parse_string));

    let mut values = HashMap::new();
    if let Some(children) = directive.children {
        for directive in children {
            let name = directive.name.clone();
            if let Some(command) = commands.get(name.as_str()) {
                match command(directive)? {
                    value @ Value::Pattern(..) => {
                        if let Some(exist_value) = values.get_mut(&name) {
                            if let Value::Nodes(childern) = exist_value {
                                childern.push(Arc::new(Node::new(value)));
                            } else {
                                return Err(anyhow!("unexpected value type"));
                            }
                        } else {
                            values.insert(name, Value::Nodes(vec![Arc::new(Node::new(value))]));
                        }
                    }
                    value => {
                        values.insert(name, value);
                    }
                }
            } else {
                return Err(anyhow!("unknown directive {}", name));
            }
        }
    }

    if !values.contains_key("listen") {
        return Err(anyhow!("server directive must have listen directive"));
    }
    if !values.contains_key("resolver") {
        return Err(anyhow!("server directive must have resolver directive"));
    }
    if !values.contains_key("ssl_certificate") {
        return Err(anyhow!(
            "server directive must have ssl_certificate directive"
        ));
    }
    if !values.contains_key("ssl_certificate_key") {
        return Err(anyhow!(
            "server directive must have ssl_certificate_key directive"
        ));
    }

    Ok(Value::ValueMap(values))
}
