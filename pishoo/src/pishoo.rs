//! Identity-bound HTTP services, components and management routes.
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

#[cfg(test)]
#[path = "../tests/support/network_credentials.rs"]
mod test_credentials;

pub use error::Error;
pub use package::{validate_lib, validate_openapi};
pub use sandbox::WasmRuntime;
pub use server::run;

type Body = dhttp::Body;
type Result<T> = std::result::Result<T, Error>;
