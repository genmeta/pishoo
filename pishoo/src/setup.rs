//! Validate configuration and the exact component snapshot to be compiled.
use std::collections::HashSet;

use dhttp_home::identity::IdentityProfile;
use rusqlite::{Connection, OpenFlags, types::ValueRef};
use wasmparser::{Encoding, Parser, Payload};

use crate::{Error, Result};

#[derive(Clone, Debug)]
pub(crate) struct ServerConfig {
    pub(crate) listen: u8,
    pub(crate) proxy_locations: Vec<ProxyLocation>,
}
#[derive(Debug)]
pub(crate) struct ProxyLocation {
    pub(crate) location: String,
    pub(crate) proxy_pass: http::uri::Parts,
}
impl Clone for ProxyLocation {
    fn clone(&self) -> Self {
        let mut proxy_pass = http::uri::Parts::default();
        proxy_pass.scheme = self.proxy_pass.scheme.clone();
        proxy_pass.authority = self.proxy_pass.authority.clone();
        proxy_pass.path_and_query = self.proxy_pass.path_and_query.clone();
        Self {
            location: self.location.clone(),
            proxy_pass,
        }
    }
}

include!("setup/config.rs");
include!("setup/manifest.rs");

#[cfg(test)]
#[path = "../tests/unit/setup.rs"]
mod tests;
