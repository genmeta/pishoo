//! Shutdown orchestration — SigTerm/SigInt/SigQuit handler logic.

use std::sync::Arc;

use nix::sys::signal::Signal;

use crate::hypervisor::state::RootState;

/// Run the shutdown sequence after receiving a termination signal.
///
/// 1. Forward the signal to all workers.
/// 2. Wait up to 2 seconds for workers to exit gracefully.
/// 3. If workers remain, SIGKILL them and wait another 2 seconds.
pub async fn run_shutdown(state: &Arc<RootState>, forwarded: Signal) {
    state.forward_unix_signal(forwarded).await;

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let exited = state.collect_exited_workers().await;
        for pid in exited {
            state
                .cleanup_worker_with_reason(pid, "signal_terminate")
                .await;
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
        let force_killed = state.force_kill_workers("shutdown_timeout").await;
        if !force_killed.is_empty() {
            tracing::warn!(
                workers = force_killed.len(),
                "force-killed lingering workers after shutdown timeout"
            );

            let force_kill_deadline =
                tokio::time::Instant::now() + std::time::Duration::from_secs(2);
            loop {
                let exited = state.collect_exited_workers().await;
                for pid in exited {
                    state
                        .cleanup_worker_with_reason(pid, "forced_shutdown")
                        .await;
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
    tracing::info!("shutdown complete");
}
