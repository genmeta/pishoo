use std::collections::HashMap;

use anyhow::{Result, anyhow};
use misc_conf::{ast::Directive, nginx::Nginx};

use super::{
    ParseFn, Value, parse_header, parse_header_always, parse_path, parse_ssh_login,
    parse_ssh_ssl_user, parse_string, parse_string_vec, parse_types, pattern::parse_pattern,
};

pub(super) fn parse_location(directive: Directive<Nginx>) -> Result<Value> {
    let mut commands: HashMap<&'static str, ParseFn> = HashMap::new();

    commands.insert("types", Box::new(parse_types));
    commands.insert("root", Box::new(parse_path));
    commands.insert("alias", Box::new(parse_path));
    commands.insert("index", Box::new(parse_string_vec));
    commands.insert("add_header", Box::new(parse_header_always));
    commands.insert("proxy_set_header", Box::new(parse_header));
    commands.insert("proxy_pass", Box::new(parse_string));
    commands.insert("ssh_login", Box::new(parse_ssh_login));
    commands.insert("ssh_ssl_user", Box::new(parse_ssh_ssl_user));
    commands.insert("ssh_deny", Box::new(parse_string_vec));

    let pattern = parse_pattern(&directive.args)?;
    let mut values = HashMap::new();
    if let Some(children) = directive.children {
        for directive in children {
            let name = directive.name.clone();
            if let Some(command) = commands.get(name.as_str()) {
                match command(directive)? {
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
                    Value::SshSslUser(ssl_user) => {
                        if let Some(exist_value) = values.get_mut(&name) {
                            if let Value::SshSslUser(exist_header) = exist_value {
                                exist_header.extend(ssl_user);
                            } else {
                                return Err(anyhow!("unexpected value type"));
                            }
                        } else {
                            values.insert(name, Value::SshSslUser(ssl_user));
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
