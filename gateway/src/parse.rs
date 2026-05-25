use std::{path::Path, sync::Arc};

use conf::parse_conf;
use dhttp_config::identity::IdentityConfig;
use misc_conf::{
    ast::{Directive, DirectiveTrait},
    nginx::Nginx,
};
use snafu::ResultExt;

use crate::error::Whatever;

// ---------------------------------------------------------------------------
// Submodules
// ---------------------------------------------------------------------------

pub mod ast;
mod commands;
pub mod conf;
pub mod directives;
mod location;
pub mod node;
pub mod pattern;
mod pishoo;
mod proxy;
pub(crate) mod server;
pub mod source;
pub mod types;
pub mod value;

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------
// Re-exports — keep the public API surface unchanged
// ---------------------------------------------------------------------------

pub use commands::Commands;
pub use node::Node;
pub use types::*;
pub use value::Value;

// ---------------------------------------------------------------------------
// Internal type aliases
// ---------------------------------------------------------------------------

pub(crate) type Result<T, E = Whatever> = std::result::Result<T, E>;

pub(crate) type ParseFn = fn(Directive<Nginx>) -> Result<Value>;

// ---------------------------------------------------------------------------
// Thread-local (to be cleaned up in Phase 3.2)
// ---------------------------------------------------------------------------

thread_local! {
    pub(crate) static IDENTITY_HOME: std::cell::RefCell<Option<IdentityConfig>> = const { std::cell::RefCell::new(None) };
}

// ---------------------------------------------------------------------------
// Top-level parse entry points
// ---------------------------------------------------------------------------

pub fn parse(configure: &[u8], root: Option<&Path>) -> Result<Arc<Node>> {
    IDENTITY_HOME.with(|r| *r.borrow_mut() = None);

    let mut directives =
        Directive::<Nginx>::parse(configure).whatever_context("cannot parse configuration")?;

    // 预处理
    if let Some(root) = root {
        directives = directives
            .into_iter()
            .map(|mut directive| directive.resolve_include(root).map(|_| directive))
            .collect::<Result<Vec<_>, _>>()
            .whatever_context("cannot resolve include in configuration")?;
    } else {
        tracing::warn!("config file has no parent, unable to resolve includes");
    }

    // 解析配置
    parse_conf(directives)
}

pub fn parse_server_config(configure: &[u8], identity_home: &IdentityConfig) -> Result<Arc<Node>> {
    IDENTITY_HOME.with(|r| *r.borrow_mut() = Some(identity_home.clone()));

    let mut directives =
        Directive::<Nginx>::parse(configure).whatever_context("cannot parse configuration")?;

    let root = identity_home.path();
    directives = directives
        .into_iter()
        .map(|mut directive| directive.resolve_include(root).map(|_| directive))
        .collect::<Result<Vec<_>, _>>()
        .whatever_context("cannot resolve include in configuration")?;

    conf::parse_server_conf(directives)
}
