use std::collections::HashMap;

use anyhow::{Result, anyhow};
use misc_conf::{ast::Directive, nginx::Nginx};

use super::{
    ParseFn, Value, parse_header, parse_header_allways, parse_path, parse_string, parse_string_map,
    parse_string_vec,
};
use crate::parse::pattern::parse_pattern;

pub(super) fn parse_location(directive: Directive<Nginx>) -> Result<Value> {
    let mut sub_parser: HashMap<&'static str, ParseFn> = HashMap::new();

    sub_parser.insert("proxy_pass", Box::new(parse_string));
    sub_parser.insert("root", Box::new(parse_path));
    sub_parser.insert("alias", Box::new(parse_path));
    sub_parser.insert("index", Box::new(parse_string_vec));
    sub_parser.insert("proxy_set_header", Box::new(parse_header));
    sub_parser.insert("add_header", Box::new(parse_header_allways));
    sub_parser.insert("types", Box::new(parse_string_map));

    let pattern = parse_pattern(&directive.args)?;
    let mut values = HashMap::new();
    if let Some(children) = directive.children {
        for directive in children {
            let name = directive.name.clone();
            if let Some(parser) = sub_parser.get(name.as_str()) {
                match parser(directive)? {
                    Value::Header(header) => {
                        if let Some(exist_value) = values.get_mut(&name) {
                            if let Value::Header(exist_header) = exist_value {
                                exist_header.extend(header);
                            } else {
                                return Err(anyhow!("unexpected value type"));
                            }
                        } else {
                            values.insert(name, Value::Header(header));
                        }
                    }
                    Value::HeaderAllways(header) => {
                        if let Some(exist_vec) = values.get_mut(&name) {
                            if let Value::HeaderAllways(exist_header) = exist_vec {
                                exist_header.extend(header);
                            } else {
                                return Err(anyhow!("unexpected value type"));
                            }
                        } else {
                            values.insert(name, Value::HeaderAllways(header));
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

    Ok(Value::Pattern(pattern, values))
}
