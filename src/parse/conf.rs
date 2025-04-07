use std::{collections::HashMap, sync::Arc};

use anyhow::{Result, anyhow};
use misc_conf::{ast::Directive, nginx::Nginx};

use super::{Node, ParseFn, Value, pishoo::parse_pishoo};

pub fn parse_conf(directives: Vec<Directive<Nginx>>) -> Result<Arc<Node>> {
    let mut sub_parser: HashMap<&'static str, ParseFn> = HashMap::new();

    sub_parser.insert("pishoo", Box::new(parse_pishoo));

    let mut values = HashMap::new();
    for directive in directives {
        let name = directive.name.clone();
        if let Some(parser) = sub_parser.get(name.as_str()) {
            match parser(directive)? {
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

    let root = Arc::new(Node::new(Value::ValueMap(values)));

    put_parent(&root);

    Ok(root)
}

fn put_parent(node: &Arc<Node>) {
    if let Value::ValueMap(map) = node.value() {
        for value in map.values() {
            if let Value::Nodes(childern) = value {
                for child in childern {
                    child.set_parent(Some(Arc::downgrade(node)));
                    put_parent(child);
                }
            }
        }
    }
}
