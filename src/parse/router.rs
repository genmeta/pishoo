use regex::Regex;

use super::{location::Location, pattern::Pattern, rule::Rule};
use crate::error::{CustomError, Result};

#[derive(Debug, Clone, Default)]
pub struct Router {
    locations: Vec<Location>,
}

impl Router {
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

    pub fn route(&self, path: &str) -> Result<(String, &Rule)> {
        for location in &self.locations {
            match &location.pattern {
                Pattern::Exact(p) => {
                    if path == p {
                        return Ok((p.to_string(), &location.rules));
                    }
                }
                Pattern::Prefix(p) => {
                    // TODO 提前检测正则是否合法

                    let p = format!(r"^{}", p);
                    let re = Regex::new(&p)?;
                    if let Some(captures) = re.captures(path) {
                        // 获取匹配的部分
                        if let Some(matched) = captures.get(0) {
                            return Ok((matched.as_str().to_string(), &location.rules));
                        }
                    }
                }
                Pattern::Regex(p) => {
                    // TODO 提前检测正则是否合法

                    let re = Regex::new(p)?;
                    if let Some(captures) = re.captures(path) {
                        // 获取匹配的部分
                        if let Some(matched) = captures.get(0) {
                            return Ok((matched.as_str().to_string(), &location.rules));
                        }
                    }
                }
                Pattern::CRegex(p) => {
                    // TODO 提前检测正则是否合法

                    let p = format!(r"(?i){}", p);
                    let re = Regex::new(&p)?;
                    if let Some(captures) = re.captures(path) {
                        // 获取匹配的部分
                        if let Some(matched) = captures.get(0) {
                            return Ok((matched.as_str().to_string(), &location.rules));
                        }
                    }
                }
                Pattern::NormalPrefix(p) => {
                    if path.starts_with(p) {
                        return Ok((p.to_string(), &location.rules));
                    }
                }
                Pattern::Common => {
                    return Ok((path.to_string(), &location.rules));
                }
            }
        }
        Err(CustomError::RouterNotFound("Not found".to_string()))
    }
}
