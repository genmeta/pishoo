//! Nginx location block parser
//!
//! Handles parsing of location directives and their configuration rules

use std::collections::HashMap;

use misc_conf::{ast::Directive, nginx::Nginx};

use super::{
    pattern::{Pattern, parse_pattern},
    rule::{Rule, parse_rule},
};
use crate::error::{CustomError, Result};

#[derive(Debug, Clone)]
pub enum Location {
    Proxy(ProxyLocation),
    Root(FileLocation),
    Alias(FileLocation),
}

#[derive(Debug, Clone)]
pub struct ProxyLocation {
    pub proxy_pass: String,
    pub add_header: Vec<(String, String)>,
    pub proxy_set_header: Vec<(String, String)>,
}

impl ProxyLocation {
    pub fn new(proxy_pass: String) -> Self {
        Self {
            proxy_pass,
            add_header: vec![],
            proxy_set_header: vec![],
        }
    }
}

#[derive(Debug, Clone)]
pub struct FileLocation {
    pub replace: String,
    pub mime_types: HashMap<String, String>,
    pub default_type: Option<String>,
    pub index: Vec<String>,
}

impl FileLocation {
    pub fn new(replace: String) -> Self {
        Self {
            replace,
            mime_types: HashMap::new(),
            default_type: None,
            index: vec![],
        }
    }
}

impl Location {
    pub fn parse(location: Directive<Nginx>) -> Result<(Pattern, Self)> {
        let pattern = parse_pattern(&location.args)?;

        let mut type_rule = None;
        let mut nomal_rule = vec![];

        for rule in location.children.into_iter().flatten().flat_map(parse_rule) {
            match rule {
                Rule::ProxyPass(_) | Rule::Root(_) | Rule::Alias(_) => {
                    if type_rule.is_none() {
                        type_rule = Some(rule);
                    }
                }
                _ => {
                    nomal_rule.push(rule);
                }
            }
        }

        match type_rule {
            Some(rule) => match rule {
                Rule::ProxyPass(proxy_pass) => {
                    let mut location = ProxyLocation::new(proxy_pass);
                    for rule in nomal_rule {
                        match rule {
                            Rule::AddHeader(name, value) => {
                                location.add_header.push((name, value));
                            }
                            Rule::ProxySetHeader(name, value) => {
                                location.proxy_set_header.push((name, value));
                            }
                            _ => {
                                return Err(CustomError::InvalidConfig(
                                    "location must have proxy_pass root or alias".to_string(),
                                ));
                            }
                        }
                    }
                    Ok((pattern, Location::Proxy(location)))
                }
                Rule::Root(root) => {
                    let mut location = FileLocation::new(root);
                    for rule in nomal_rule {
                        match rule {
                            Rule::MimeTypes(mime_type) => {
                                location.mime_types = mime_type;
                            }
                            Rule::DefaultType(default_type) => {
                                location.default_type = Some(default_type);
                            }
                            _ => {
                                return Err(CustomError::InvalidConfig(
                                    "location must have mime_type and default_type".to_string(),
                                ));
                            }
                        }
                    }
                    Ok((pattern, Location::Root(location)))
                }
                Rule::Alias(alias) => {
                    let mut location = FileLocation::new(alias);
                    for rule in nomal_rule {
                        match rule {
                            Rule::MimeTypes(mime_type) => {
                                location.mime_types = mime_type;
                            }
                            Rule::DefaultType(default_type) => {
                                location.default_type = Some(default_type);
                            }
                            _ => {
                                return Err(CustomError::InvalidConfig(
                                    "location must have mime_type and default_type".to_string(),
                                ));
                            }
                        }
                    }
                    Ok((pattern, Location::Alias(location)))
                }
                _ => Err(CustomError::InvalidConfig(
                    "location must have proxy_pass root or alias".to_string(),
                )),
            },
            None => Err(CustomError::MissingConfig(
                "location must have proxy_pass root or alias".to_string(),
            )),
        }
    }
}
