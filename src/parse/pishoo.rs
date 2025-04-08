use std::{collections::HashMap, sync::Arc};

use anyhow::{Result, anyhow};
use misc_conf::{ast::Directive, nginx::Nginx};

use super::{
    Node, ParseFn, Value, parse_string, parse_string_map, proxy::parse_proxy, server::parse_server,
};

pub(super) fn parse_pishoo(directive: Directive<Nginx>) -> Result<Value> {
    let mut commands: HashMap<&'static str, ParseFn> = HashMap::new();

    commands.insert("types", Box::new(parse_string_map));
    commands.insert("default_type", Box::new(parse_string));
    commands.insert("server", Box::new(parse_server));
    commands.insert("proxy", Box::new(parse_proxy));

    let mut values = HashMap::new();
    if let Some(children) = directive.children {
        for directive in children {
            let name = directive.name.clone();
            if let Some(command) = commands.get(name.as_str()) {
                match command(directive)? {
                    value @ Value::ValueMap(..) => {
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

    Ok(Value::ValueMap(values))
}
