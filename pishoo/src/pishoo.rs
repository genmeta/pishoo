//! Identity-bound HTTP services, components and management routes.
mod daemon;
mod error;
mod exec;
mod routes;
mod sandbox;
mod setup;

pub use daemon::run;
pub use error::Error;
pub use sandbox::validate_lib;

type Body = dhttp::Body;
type Result<T> = std::result::Result<T, Error>;
