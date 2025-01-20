use regex::Regex;

use super::{location::Location, pattern::Pattern, rule::Rule};
use crate::error::Result;

#[derive(Debug, Clone)]
pub struct Router {
    locations: Vec<Location>,
}

impl Router {
    pub fn new() -> Router {
        Router {
            locations: Vec::new(),
        }
    }

    pub fn insert(&mut self, location: Location) -> Result<()> {
        let priority = location.pattern.priority();
        let pos = self
            .locations
            .iter()
            .position(|location| location.pattern.priority() > priority)
            .unwrap_or(self.locations.len());
        self.locations.insert(pos, location);
        Ok(())
    }

    pub fn route(&self, path: &str) -> Result<(String, Vec<Rule>)> {
        let mut pattern = String::new();
        let mut rules = Vec::new();

        for location in &self.locations {
            match &location.pattern {
                Pattern::Exact(p) => {
                    if path == p {
                        pattern = p.to_string();
                        rules = location.rules.clone();
                        break;
                    }
                }
                Pattern::Prefix(p) => {
                    // TODO 提前检测正则是否合法

                    let p = format!(r"^{}", p);
                    let re = Regex::new(&p)?;
                    if let Some(captures) = re.captures(path) {
                        // 获取匹配的部分
                        if let Some(matched) = captures.get(0) {
                            pattern = matched.as_str().to_string();
                        }
                        rules = location.rules.clone();
                        break;
                    }
                }
                Pattern::Regex(p) => {
                    // TODO 提前检测正则是否合法

                    let re = Regex::new(p)?;
                    if let Some(captures) = re.captures(path) {
                        // 获取匹配的部分
                        if let Some(matched) = captures.get(0) {
                            pattern = matched.as_str().to_string();
                        }
                        rules = location.rules.clone();
                        break;
                    }
                }
                Pattern::CRegex(p) => {
                    // TODO 提前检测正则是否合法

                    let p = format!(r"(?i){}", p);
                    let re = Regex::new(&p)?;
                    if let Some(captures) = re.captures(path) {
                        // 获取匹配的部分
                        if let Some(matched) = captures.get(0) {
                            pattern = matched.as_str().to_string();
                        }
                        rules = location.rules.clone();
                        break;
                    }
                }
                Pattern::NormalPrefix(p) => {
                    if path.starts_with(p) {
                        pattern = p.to_string();
                        rules = location.rules.clone();
                        break;
                    }
                }
                Pattern::Common => {
                    pattern = path.to_string();
                    rules = location.rules.clone();
                    break;
                }
            }
        }

        Ok((pattern, rules))
    }
}
