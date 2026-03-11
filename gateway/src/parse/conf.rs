use std::sync::Arc;

use misc_conf::{ast::Directive, nginx::Nginx};

use crate::parse::{pishoo::parse_pishoo, Commands, Node, Result, Value};

pub fn parse_conf(directives: Vec<Directive<Nginx>>) -> Result<Arc<Node>> {
    let mut commands = Commands::new();

    commands.insert("pishoo", parse_pishoo);

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
