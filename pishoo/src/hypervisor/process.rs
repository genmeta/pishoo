//! Worker process spawning, monitoring, and signal forwarding.
//!
//! Spawns `pishoo-worker` binaries, establishes remoc IPC channels with the
//! new [`ControlPlane`](crate::ipc::ControlPlane) RTC trait, and
//! runs a monitor loop to detect and clean up exited workers.

mod batch;
mod monitor;
mod spawn;

pub use batch::spawn_configured_workers;
pub use monitor::spawn_monitor_loop;
pub use spawn::{SpawnWorkerError, SpawnedWorker, spawn_worker};
