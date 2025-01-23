use super::{location::Location, rule::Rule};
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
            if let Some(matched) = location.pattern.try_match(path)? {
                return Ok((matched, &location.rules));
            }
        }
        Err(CustomError::RouterNotFound(path.to_string()))
    }
}
