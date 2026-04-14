//! Worker binary resolution and batch spawning.

use std::{path::PathBuf, sync::Arc};

use snafu::Report;

use super::spawn::spawn_worker;
use crate::hypervisor::state::RootState;

/// Resolve the path of the `pishoo-worker` binary.
///
/// Search order:
/// 1. Runtime env var `PISHOO_WORKER_BIN`
/// 2. Compile-time env var `PISHOO_WORKER_BIN` (set by deb builds)
/// 3. `<exe_dir>/../libexec/pishoo-worker` (Homebrew layout)
/// 4. `<exe_dir>/pishoo-worker` (debug / same-dir fallback)
pub(crate) fn worker_binary_path() -> PathBuf {
    // 1. Runtime environment variable
    if let Ok(path) = std::env::var("PISHOO_WORKER_BIN") {
        return PathBuf::from(path);
    }

    // 2. Compile-time environment variable (set during release deb builds)
    if let Some(path) = option_env!("PISHOO_WORKER_BIN") {
        return PathBuf::from(path);
    }

    if let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    {
        // 3. Homebrew libexec layout: <prefix>/bin/pishoo → <prefix>/libexec/pishoo-worker
        let libexec = exe_dir.join("../libexec/pishoo-worker");
        if libexec.exists() {
            return libexec;
        }

        // 4. Same directory (debug builds, Windows, cargo build output)
        return exe_dir.join("pishoo-worker");
    }

    PathBuf::from("pishoo-worker")
}

/// Spawn worker processes for all resolved targets.
///
/// Failures are per-user and logged; they do not prevent other workers
/// from being spawned.
pub async fn spawn_configured_workers(
    state: &Arc<RootState>,
    worker_targets: Vec<crate::config::ResolvedWorkerTarget>,
) {
    if worker_targets.is_empty() {
        tracing::info!("no worker targets resolved");
        return;
    }

    let total = worker_targets.len();
    let mut spawned = 0usize;
    let worker_bin = worker_binary_path();

    for target in &worker_targets {
        match spawn_worker(&worker_bin, target, state.clone()).await {
            Ok(_) => spawned += 1,
            Err(error) => {
                // Worker spawn failures (fork/exec, IPC negotiation, hello
                // timeout) are per-user problems that must not bring down the
                // entire root process.  Log and continue to the next worker.
                tracing::error!(
                    user = %target.name,
                    error = %Report::from_error(&error),
                    "failed to spawn worker, skipping user"
                );
            }
        };
    }

    let failed = total - spawned;
    if failed > 0 {
        tracing::warn!(
            total,
            spawned,
            failed,
            "worker spawn complete with failures"
        );
    } else {
        tracing::info!(count = total, "all workers spawned");
    }
}
