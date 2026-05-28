//! Root process modules.
//!
//! These modules are only linked into the root binary (`pishoo`).

pub(crate) mod endpoint_factory;
pub mod ipc_server;
pub mod launcher;
pub mod local_plane;
pub mod local_service;
pub mod log;
pub mod process;
pub mod reload;
pub mod shutdown;
pub mod signal;
pub mod state;
pub mod task_scope;
pub mod worker_handle;
