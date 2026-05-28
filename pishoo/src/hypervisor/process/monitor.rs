//! Worker process monitor loop.

use std::sync::Arc;

use snafu::Report;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use super::{batch::worker_binary_path, spawn::spawn_worker};
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
    let restartable = failure.error.is_restartable();
    let Some(summary) = state.cleanup_worker(failure.pid, failure.error).await else {
        return;
    };

    if !restartable || state.pid_for_uid(summary.uid).await.is_some() {
        return;
    }

    let Some(target) = state.desired_worker_target(summary.uid).await else {
        return;
    };

    let worker_bin = worker_binary_path();
    if let Err(error) = spawn_worker(&worker_bin, &target, state.clone()).await {
        tracing::error!(
            uid = summary.uid.as_raw(),
            user = %target.name,
            error = %Report::from_error(&error),
            "failed to restart worker"
        );
    }
}

/// Spawn the monitor loop as a background task. Returns the join handle.
pub fn spawn_monitor_loop(state: Arc<RootState>, shutdown: CancellationToken) -> JoinHandle<()> {
    tokio::spawn(run_monitor_loop(state, shutdown).in_current_span())
}
