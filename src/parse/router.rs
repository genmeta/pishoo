//! Request routing module
//!
//! Implements priority-based routing to location blocks

use super::{location::Location, pattern::Pattern};
use crate::error::{CustomError, Result};

#[derive(Debug, Clone, Default)]
pub struct Router {
    pub locations: Vec<(Pattern, Location)>,
}

impl Router {
    pub fn insert(&mut self, pattern: Pattern, location: Location) {
        let priority = pattern.priority();
        let pos = self
            .locations
            .iter()
            .position(|(pattern, _)| pattern.priority() > priority)
            .unwrap_or(self.locations.len());
        self.locations.insert(pos, (pattern, location));
    }

    pub fn route(&self, path: &str) -> Result<(String, &Location)> {
        self.locations
            .iter()
            .find_map(|(pattern, location)| {
                pattern
                    .try_match(path)
                    .ok()
                    .flatten()
                    .map(|matched| (matched, location))
            })
            .ok_or_else(|| CustomError::RouterNotFound(path.to_string()))
    }
}
