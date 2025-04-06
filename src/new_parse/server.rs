use std::{collections::HashMap, sync::Arc};

use anyhow::{Result, anyhow};
use misc_conf::{ast::Directive, nginx::Nginx};

use super::{
    ParseFn, ParseNode, ParseValue, location::parse_location, parse_address, parse_path,
    parse_string_vec,
};

pub(super) fn parse_server(directive: Directive<Nginx>) -> Result<ParseValue> {
    let mut sub_parser: HashMap<&'static str, ParseFn> = HashMap::new();

    sub_parser.insert("listen", Box::new(parse_address));
    sub_parser.insert("server_name", Box::new(parse_string_vec));
    sub_parser.insert("resolver", Box::new(parse_address));
    sub_parser.insert("ssl_certificate", Box::new(parse_path));
    sub_parser.insert("ssl_certificate_key", Box::new(parse_path));
    sub_parser.insert("location", Box::new(parse_location));

    let mut values = HashMap::new();
    if let Some(children) = directive.children {
        for directive in children {
            let name = directive.name.clone();
            if let Some(parser) = sub_parser.get(name.as_str()) {
                match parser(directive)? {
                    value @ ParseValue::Location(..) => {
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
