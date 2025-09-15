use std::sync::Arc;

use misc_conf::{ast::Directive, nginx::Nginx};

use crate::parse::{Commands, Node, Result, Value, parse_string, pishoo::parse_pishoo};

pub fn parse_conf(directives: Vec<Directive<Nginx>>) -> Result<Arc<Node>> {
    let mut commands = Commands::new();

    commands.insert("pishoo", parse_pishoo);
    commands.insert("pid", parse_string);

    let values = commands.parse(directives)?;
    let root = Arc::new(Node::new(Value::ValueMap(values)));
    put_parent_recursively(&root);

    Ok(root)
}

fn put_parent_recursively(node: &Arc<Node>) {
    if let Value::ValueMap(map) = node.value() {
        for value in map.values() {
            if let Value::Nodes(children) = value {
                for child in children {
                    child.set_parent(Some(Arc::downgrade(node)));
                    put_parent_recursively(child);
                }
            }
        }
    }
}
