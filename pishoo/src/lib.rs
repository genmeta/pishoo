//! Identity-bound HTTP services, components and management routes.
mod daemon;
mod error;
mod routes;
mod setup;
mod terminal;
mod wasm;

pub use daemon::{DaemonConfig, run};
pub use error::Error;
pub use setup::validate_lib;
pub use terminal::TerminalPolicy;

type Body = dhttp::Body;
type Result<T> = std::result::Result<T, Error>;
