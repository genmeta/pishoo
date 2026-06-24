//! Shared service plumbing for identity and pishoo config services.
//!
//! Submodules:
//! - [`accept`]: per-server accept loop and listener swap.
//! - [`runtime`]: [`runtime::ServerRuntime`] / [`runtime::RuntimeRegistry`] /
//!   [`runtime::WorkerRuntime`] — lifecycle and diff-based source application.
//! - [`snapshot`]: [`snapshot::ServerService`] — immutable per-server
//!   configuration handed to accept loops.
//! - [`source`]: [`source::ServerSource`] enum and per-variant loaders.

pub mod accept;
pub mod runtime;
pub mod snapshot;
pub mod source;
