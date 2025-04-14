#![feature(ip)]

use std::io::{self};

mod command;
mod common;
pub mod dns;
pub mod error;
pub mod forward;
pub mod localhost;
pub mod parse;
pub mod reverse;

use async_trait::async_trait;
pub use gm_quic::EndpointAddr;

#[async_trait]
pub trait Resolver {
    async fn publish(&self, name: &str, addresses: &[EndpointAddr]) -> io::Result<()>;
    async fn look_up(&self, name: &str) -> io::Result<Vec<EndpointAddr>>;
}
