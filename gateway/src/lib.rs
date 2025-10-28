#![feature(ip)]

mod command;
mod common;
pub mod error;
pub mod forward;
mod h3;
pub mod parse;
mod pool;
mod publisher;
pub mod reverse;

use std::sync::Arc;

pub use gm_quic::EndpointAddr;
use qtraversal::iface::TraversalFactory;

fn traversal_factory() -> &'static Arc<TraversalFactory> {
    let agents = [
        "1.12.74.4:20004".parse().unwrap(),
        "[2402:4e00:c011:1700:8624:7e0:5c9a:2]:20004"
            .parse()
            .unwrap(),
    ];
    TraversalFactory::initialize_global(agents.to_vec()).unwrap()
}
