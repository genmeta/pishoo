//! Reload orchestration — SigHup handler logic.

use std::{path::Path, sync::Arc};

use nix::sys::signal::Signal;
use snafu::Report;
use tracing::Instrument;

use crate::{
    config::ResolvedWorkerTarget,
    hypervisor::{
        local_service::LocalServiceHandle,
        state::{RootState, WorkerProcessError},
    },
};

/// Run the full reload sequence:
///
/// 1. Preflight: load and validate the new configuration.
/// 2. Replace root-local servers.
/// 3. Compute worker diff (unchanged / added / removed / changed).
/// 4. Kill removed + changed workers (parallel SIGTERM + grace).
/// 5. Scrub conflicted server names.
/// 6. Forward SIGHUP to unchanged workers.
/// 7. Spawn added + changed workers.
///
/// On preflight or local-service failure, the reload is aborted and the
/// previous state is preserved.
pub async fn run_reload(
    state: &Arc<RootState>,
    config_file: &Path,
    current_worker_targets: &mut Vec<ResolvedWorkerTarget>,
    local_service_handle: &mut Option<LocalServiceHandle>,
) {
    tracing::info!("received reload signal");

    let next_snapshot = match super::load_root_reload_snapshot(config_file).await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            tracing::warn!(
                error = %Report::from_error(&error),
                path = %config_file.display(),
                "reload preflight failed; keeping current root state"
            );
            return;
        }
    };

    if let Err(error) = crate::hypervisor::local_service::replace_local_service(
        state,
        local_service_handle,
        &next_snapshot.entry_config,
    )
    .await
    {
        tracing::warn!(
            error = %Report::from_error(&error),
            "failed to reload root-local servers; keeping previous worker state"
        );
        return;
    }

    // Compute worker diff.
    let diff =
        crate::config::compute_worker_diff(current_worker_targets, &next_snapshot.worker_targets);

    fn target_names(targets: &[ResolvedWorkerTarget]) -> Vec<&str> {
        targets.iter().map(|t| t.name.as_str()).collect()
    }
    tracing::info!(
        unchanged = ?target_names(&diff.unchanged),
        added = ?target_names(&diff.added),
        removed = ?target_names(&diff.removed),
        changed = ?diff.changed.iter().map(|(_, new)| new.name.as_str()).collect::<Vec<_>>(),
        "reload diff"
    );

    state
        .set_desired_workers(next_snapshot.worker_targets.clone())
        .await;

    // Phase 1: Kill removed + changed workers (parallel).
    if !diff.removed.is_empty() || !diff.changed.is_empty() {
        let mut kill_tasks = tokio::task::JoinSet::new();
        for target in &diff.removed {
            let state = state.clone();
            let uid = target.uid;
            kill_tasks.spawn(
                async move {
                    if let Some(pid) = state.pid_for_uid(uid).await {
                        let error = WorkerProcessError::ReloadRemoved;
                        // SIGTERM + 2s grace
                        state.send_signal_to_user(uid, Signal::SIGTERM).await;
                        if !state
                            .wait_worker_exit(pid, std::time::Duration::from_secs(2))
                            .await
                        {
                            state.force_kill_worker(pid, &error).await;
                            state
                                .wait_worker_exit(pid, std::time::Duration::from_secs(2))
                                .await;
                        }
                        state.cleanup_worker(pid, error).await;
                    }
                }
                .in_current_span(),
            );
        }
        for (old, _) in &diff.changed {
            let state = state.clone();
            let uid = old.uid;
            kill_tasks.spawn(
                async move {
                    if let Some(pid) = state.pid_for_uid(uid).await {
                        let error = WorkerProcessError::ReloadChanged;
                        // SIGTERM + 2s grace
                        state.send_signal_to_user(uid, Signal::SIGTERM).await;
                        if !state
                            .wait_worker_exit(pid, std::time::Duration::from_secs(2))
                            .await
                        {
                            state.force_kill_worker(pid, &error).await;
                            state
                                .wait_worker_exit(pid, std::time::Duration::from_secs(2))
                                .await;
                        }
                        state.cleanup_worker(pid, error).await;
                    }
                }
                .in_current_span(),
            );
        }
        while kill_tasks.join_next().await.is_some() {}
    }

    // Scrub conflicted names before forwarding reload to workers.
    state.clear_listener_poison().await;

    // Phase 2: Forward SIGHUP to unchanged workers.
    for target in &diff.unchanged {
        state.send_signal_to_user(target.uid, Signal::SIGHUP).await;
    }

    let mut missing_unchanged_workers = Vec::new();
    for target in &diff.unchanged {
        if state.pid_for_uid(target.uid).await.is_none() {
            missing_unchanged_workers.push(target.clone());
        }
    }

    let failed_desired_workers = state.take_failed_desired_workers().await;

    // Phase 3: Spawn added + changed workers, unchanged desired workers that
    // were already cleaned up before this reload finished, and any desired
    // workers parked in the failed registry from prior restartable failures.
    let workers_to_spawn: Vec<_> = diff
        .added
        .into_iter()
        .chain(diff.changed.into_iter().map(|(_old, new)| new))
        .chain(missing_unchanged_workers)
        .chain(failed_desired_workers)
        .collect();
    if !workers_to_spawn.is_empty() {
        crate::hypervisor::process::spawn_configured_workers(state, workers_to_spawn).await;
    }

    *current_worker_targets = next_snapshot.worker_targets;
    tracing::info!("reload complete");
}
