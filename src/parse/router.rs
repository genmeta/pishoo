use crate::error::Result;

use super::location::Location;

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

    pub fn locations(&self) -> &Vec<Location> {
        &self.locations
    }
}
