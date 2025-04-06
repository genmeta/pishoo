use std::{collections::HashMap, sync::Arc};

use anyhow::{Result, anyhow};
use misc_conf::{ast::Directive, nginx::Nginx};

use super::{
    ParseFn, ParseNode, ParseValue, parse_string, parse_string_map, proxy::parse_proxy,
    server::parse_server,
};

pub(super) fn parse_pishoo(directive: Directive<Nginx>) -> Result<ParseValue> {
    let mut sub_parser: HashMap<&'static str, ParseFn> = HashMap::new();

    sub_parser.insert("types", Box::new(parse_string_map));
    sub_parser.insert("default_type", Box::new(parse_string));
    sub_parser.insert("server", Box::new(parse_server));
    sub_parser.insert("proxy", Box::new(parse_proxy));

    let mut values = HashMap::new();
    if let Some(children) = directive.children {
        for directive in children {
            let name = directive.name.clone();
            if let Some(parser) = sub_parser.get(name.as_str()) {
                match parser(directive)? {
                    value @ ParseValue::ValueMap(..) => {
                        if let Some(exist_value) = values.get_mut(&name) {
                            if let ParseValue::Nodes(childern) = exist_value {
                                childern.push(Arc::new(ParseNode::new(value)));
                            } else {
                                return Err(anyhow!("unexpected value type"));
                            }
                        } else {
                            values.insert(
                                name,
                                ParseValue::Nodes(vec![Arc::new(ParseNode::new(value))]),
                            );
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

    Ok(ParseValue::ValueMap(values))
}
