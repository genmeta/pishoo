//! Identity-bound HTTP services, components and management routes.
mod error;
mod exec;
mod routes;
mod sandbox;
mod server;
mod setup;

pub use error::Error;
pub use sandbox::validate_lib;
pub use server::run;

type Body = dhttp::Body;
type Result<T> = std::result::Result<T, Error>;
