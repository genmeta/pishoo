//! Worker process monitor loop.

use std::sync::Arc;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::hypervisor::state::{RootState, WorkerFailure};

/// Run the worker monitor loop: wait for worker-failure notification,
/// SIGCHLD notification, or 5s fallback, then clean up failed workers.
///
/// SIGCHLD signals and IPC failure notifications may be coalesced, so we loop
/// `collect_worker_failures` until no more failures are found after each wake-up.
pub async fn run_monitor_loop(state: Arc<RootState>, shutdown: CancellationToken) {
    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            _ = state.worker_notify.notified() => {}
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
        }
        // Failure notification coalescing: loop until no more queued or
        // waitpid-observed worker failures remain.
        loop {
            let failures = state.collect_worker_failures().await;
            if failures.is_empty() {
                break;
            }
            for failure in failures {
                handle_worker_failure(&state, failure).await;
            }
        }
    }
}

async fn handle_worker_failure(state: &Arc<RootState>, failure: WorkerFailure) {
    let _ = state.cleanup_worker(failure.pid, failure.error).await;
}

/// Spawn the monitor loop as a background task. Returns the join handle.
pub fn spawn_monitor_loop(state: Arc<RootState>, shutdown: CancellationToken) -> JoinHandle<()> {
    tokio::spawn(run_monitor_loop(state, shutdown).in_current_span())
}
