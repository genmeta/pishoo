//! Shutdown orchestration — SigTerm/SigInt/SigQuit handler logic.

use std::sync::Arc;

use nix::sys::signal::Signal;

use crate::hypervisor::state::{RootState, WorkerProcessError};

/// Run the shutdown sequence after receiving a termination signal.
///
/// 1. Forward the signal to all workers.
/// 2. Wait up to 2 seconds for workers to exit gracefully.
/// 3. If workers remain, SIGKILL them and wait another 2 seconds.
pub async fn run_shutdown(state: &Arc<RootState>, forwarded: Signal) {
    state.clear_desired_workers().await;
    state.forward_unix_signal(forwarded).await;

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let failures = state.collect_worker_failures().await;
        for failure in failures {
            state.cleanup_worker(failure.pid, failure.error).await;
        }
        if state.worker_pids().await.is_empty() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    if !state.worker_pids().await.is_empty() {
        let shutdown_timeout = WorkerProcessError::ShutdownTimeout;
        let force_killed = state.force_kill_workers(&shutdown_timeout).await;
        if !force_killed.is_empty() {
            tracing::warn!(
                workers = force_killed.len(),
                "force-killed lingering workers after shutdown timeout"
            );

            let force_kill_deadline =
                tokio::time::Instant::now() + std::time::Duration::from_secs(2);
            loop {
                let failures = state.collect_worker_failures().await;
                for failure in failures {
                    state.cleanup_worker(failure.pid, failure.error).await;
                }
                if state.worker_pids().await.is_empty() {
                    break;
                }
                if tokio::time::Instant::now() >= force_kill_deadline {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    } else {
        tracing::info!("all workers exited gracefully");
    }
    for pid in state.worker_pids().await {
        state
            .cleanup_worker(pid, WorkerProcessError::ForcedShutdown)
            .await;
    }
    tracing::info!("shutdown complete");
}
