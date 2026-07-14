//! Serialized root reload orchestration.

use std::sync::Arc;

use nix::sys::signal::Signal;
use snafu::Report;
use tracing::Instrument;

use crate::{
    config::{WorkerAccount, WorkerRoster},
    hypervisor::{
        global_service::GlobalServiceHandle,
        state::{RootState, WorkerProcessError},
    },
};

pub async fn run_reload(
    state: &Arc<RootState>,
    config_source: &crate::config::PishooConfigSource,
    current_workers: &mut Vec<WorkerAccount>,
    global_service: &mut Option<GlobalServiceHandle>,
) {
    tracing::info!("received reload signal");
    let plan = match super::load_root_reload_snapshot(config_source).await {
        Ok(plan) => plan,
        Err(error) => {
            tracing::error!(
                error = %Report::from_error(&error),
                path = %config_source.config_path().display(),
                "global pishoo reload failed; entering config-failed state"
            );
            enter_config_failed(state, current_workers, global_service).await;
            return;
        }
    };

    state.clear_listener_poison().await;
    let next_workers = plan.desired_workers().clone();
    let snapshot = plan.worker_defaults().clone();

    let worker_branch = reconcile_workers(state, current_workers, next_workers, snapshot);
    let global_branch =
        crate::hypervisor::global_service::replace_global_service(state, global_service, &plan);
    tokio::join!(worker_branch, global_branch);
    tracing::info!("reload complete");
}

async fn enter_config_failed(
    state: &Arc<RootState>,
    current_workers: &mut Vec<WorkerAccount>,
    global_service: &mut Option<GlobalServiceHandle>,
) {
    if let Some(service) = global_service.take() {
        service.shutdown().await;
    }
    crate::hypervisor::shutdown::run_shutdown(state, Signal::SIGTERM).await;
    current_workers.clear();
    state.wait_resource_transitions().await;
}

async fn reconcile_workers(
    state: &Arc<RootState>,
    current_workers: &mut Vec<WorkerAccount>,
    next_roster: WorkerRoster,
    snapshot: gateway::parse::config::RootWorkerDefaultsSnapshot,
) {
    let next_workers = next_roster.to_vec();
    let diff = crate::config::compute_worker_diff(current_workers, &next_workers);
    state.set_desired_workers(next_workers.clone()).await;
    state.dispatch_worker_defaults(snapshot.clone()).await;

    let mut stopping = tokio::task::JoinSet::new();
    for target in diff
        .removed
        .iter()
        .chain(diff.changed.iter().map(|(old, _)| old))
    {
        let state = state.clone();
        let uid = target.uid();
        let reason = if diff.removed.iter().any(|removed| removed.uid() == uid) {
            WorkerProcessError::ReloadRemoved
        } else {
            WorkerProcessError::ReloadChanged
        };
        stopping.spawn(
            async move {
                let Some(pid) = state.pid_for_uid(uid).await else {
                    return;
                };
                state.send_signal_to_user(uid, Signal::SIGTERM).await;
                if !state
                    .wait_worker_exit(pid, std::time::Duration::from_secs(2))
                    .await
                {
                    state.force_kill_worker(pid, &reason).await;
                    state
                        .wait_worker_exit(pid, std::time::Duration::from_secs(2))
                        .await;
                }
                state.cleanup_worker(pid, reason).await;
            }
            .in_current_span(),
        );
    }
    while stopping.join_next().await.is_some() {}

    let mut missing_unchanged_workers = Vec::new();
    for target in &diff.unchanged {
        if state.pid_for_uid(target.uid()).await.is_none() {
            missing_unchanged_workers.push(target.clone());
        }
    }
    let failed = state.take_failed_desired_workers().await;
    let mut scheduled_uids = std::collections::HashSet::new();
    let to_spawn = diff
        .added
        .into_iter()
        .chain(diff.changed.into_iter().map(|(_, new)| new))
        .chain(missing_unchanged_workers)
        .chain(failed)
        .filter(|worker| scheduled_uids.insert(worker.uid()))
        .collect::<Vec<_>>();
    crate::hypervisor::process::spawn_configured_workers(state, to_spawn, snapshot).await;
    *current_workers = next_workers;
}
