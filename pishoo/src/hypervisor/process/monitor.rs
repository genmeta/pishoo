//! Worker process monitor loop.

use std::sync::Arc;

use tokio_util::task::AbortOnDropHandle;
use tracing::Instrument;

use crate::hypervisor::state::RootState;

/// Run the worker monitor loop: wait for SIGCHLD notification or 5s fallback,
/// then reap all exited workers.
///
/// SIGCHLD signals may be coalesced by the kernel, so we loop `collect_exited_workers`
/// until no more exits are found after each wake-up.
pub async fn run_monitor_loop(state: Arc<RootState>) {
    loop {
        tokio::select! {
            _ = state.worker_notify.notified() => {}
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
        }
        // SIGCHLD coalescing: loop waitpid until no more exited children.
        loop {
            let exited = state.collect_exited_workers().await;
            if exited.is_empty() {
                break;
            }
            for pid in exited {
                state.cleanup_worker_with_reason(pid, "child_exit").await;
            }
        }
    }
}

/// Spawn the monitor loop as a background task. Returns the join handle.
pub fn spawn_monitor_loop(state: Arc<RootState>) -> AbortOnDropHandle<()> {
    AbortOnDropHandle::new(tokio::spawn(run_monitor_loop(state).in_current_span()))
}
