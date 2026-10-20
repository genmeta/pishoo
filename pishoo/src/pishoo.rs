//! Identity-bound HTTP services, components and management routes.
mod cli;
mod contact;
mod dns;
mod error;
mod package;
mod routes;
mod sandbox;
mod server;
mod setup;

#[cfg(test)]
#[path = "../tests/support/test_identity.rs"]
mod test_identity;

pub use cli::run_command;
pub use error::Error;
pub use package::{validate_lib, validate_openapi};
pub use sandbox::WasmRuntime;
pub use server::run;

type Body = dhttp::Body;
type Result<T> = std::result::Result<T, Error>;
