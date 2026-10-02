//! Identity-bound HTTP services, components and management routes.
mod chat;
mod error;
mod exec;
mod routes;
mod sandbox;
mod server;
mod setup;
mod workspace;

pub use error::Error;
pub use sandbox::validate_lib;
pub use server::run;

type Body = dhttp::Body;
type Result<T> = std::result::Result<T, Error>;
