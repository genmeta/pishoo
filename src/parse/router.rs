use super::{location::Location, rule::Rule};
use crate::error::{CustomError, Result};

#[derive(Debug, Clone, Default)]
pub struct Router {
    locations: Vec<Location>,
}

impl Router {
    pub fn insert(&mut self, location: Location) {
        let priority = location.pattern.priority();
        let pos = self
            .locations
            .iter()
            .position(|location| location.pattern.priority() > priority)
            .unwrap_or(self.locations.len());
        self.locations.insert(pos, location);
    }

    pub fn route(&self, path: &str) -> Result<(String, &Rule)> {
        self.locations
            .iter()
            .find_map(|location| {
                location
                    .pattern
                    .try_match(path)
                    .ok()
                    .flatten()
                    .map(|matched| (matched, &location.rule))
            })
            .ok_or_else(|| CustomError::RouterNotFound(path.to_string()))
    }
}
