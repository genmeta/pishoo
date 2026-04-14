use std::{collections::HashMap, sync::Arc};

use misc_conf::{ast::Directive, nginx::Nginx};

use super::{Node, ParseFn, Result, Value};

#[derive(Default, Debug, Clone)]
pub struct Commands(HashMap<&'static str, ParseFn>);

impl Commands {
    pub fn new() -> Self {
        Self(HashMap::new())
    }

    pub fn insert(&mut self, name: &'static str, command: ParseFn) {
        self.0.insert(name, command);
    }

    pub fn parse(
        &self,
        directives: impl IntoIterator<Item = Directive<Nginx>>,
    ) -> Result<HashMap<String, Value>> {
        let mut values = HashMap::new();
        for directive in directives {
            let name = directive.name.clone();
            let Some(command) = self.0.get(name.as_str()) else {
                snafu::whatever!("unknown directive `{name}`",)
            };

            match command(directive)? {
                value @ (Value::ValueMap(..) | Value::Pattern(..)) => {
                    let Value::Nodes(nodes) =
                        values.entry(name).or_insert_with(|| Value::Nodes(vec![]))
                    else {
                        unreachable!("unexpected value type, should be `Nodes`");
                    };
                    nodes.push(Arc::new(Node::new(value)));
                }
                Value::Header(headers) => {
                    let Value::Header(exist_headers) =
                        values.entry(name).or_insert_with(|| Value::Header(vec![]))
                    else {
                        unreachable!("unexpected value type, should be `Header`");
                    };
                    exist_headers.extend(headers);
                }
                Value::SshSslUser(users) => {
                    let Value::SshSslUser(exist_users) = values
                        .entry(name)
                        .or_insert_with(|| Value::SshSslUser(vec![]))
                    else {
                        unreachable!("unexpected value type, should be `SshSslUser`");
                    };
                    exist_users.extend(users);
                }
                Value::Addr(addr) => {
                    values
                        .entry(name)
                        .and_modify(|v| match v {
                            Value::Addr(old_addr) => *v = Value::AddrVec(vec![*old_addr, addr]),
                            Value::AddrVec(vec) => vec.push(addr),
                            _ => unreachable!("unexpected value type for Addr aggregation"),
                        })
                        .or_insert(Value::Addr(addr));
                }
                value => _ = values.insert(name, value),
            }
        }
        Ok(values)
    }
}
