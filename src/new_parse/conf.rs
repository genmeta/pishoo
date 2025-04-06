use std::{collections::HashMap, sync::Arc};

use anyhow::{Result, anyhow};
use misc_conf::{ast::Directive, nginx::Nginx};

use super::{ParseFn, ParseNode, ParseValue, pishoo::parse_pishoo};

pub fn parse_conf(directives: Vec<Directive<Nginx>>) -> Result<Arc<ParseNode>> {
    let mut sub_parser: HashMap<&'static str, ParseFn> = HashMap::new();

    sub_parser.insert("pishoo", Box::new(parse_pishoo));

    let mut values = HashMap::new();
    for directive in directives {
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

    let root = Arc::new(ParseNode::new(ParseValue::ValueMap(values)));

    put_parent(&root);

    Ok(root)
}

fn put_parent(node: &Arc<ParseNode>) {
    if let ParseValue::ValueMap(map) = node.value() {
        for value in map.values() {
            if let ParseValue::Nodes(childern) = value {
                for child in childern {
                    child.set_parent(Some(Arc::downgrade(node)));
                    put_parent(child);
                }
            }
        }
    }
}
