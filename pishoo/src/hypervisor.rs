//! Root process modules.
//!
//! These modules are only linked into the root binary (`pishoo`).

pub(crate) mod endpoint_factory;
pub mod global_service;
pub mod in_process_plane;
pub mod ipc_server;
pub mod launcher;
pub mod log;
pub mod process;
pub mod reload;
pub(crate) mod resource;
pub mod shutdown;
pub mod signal;
pub mod state;
pub mod task_scope;
pub mod worker_handle;
