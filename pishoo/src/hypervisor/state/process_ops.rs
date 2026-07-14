//! Worker/process registry operations on [`RootState`].

use std::{collections::HashSet, sync::Arc};

use nix::{
    sys::{signal::Signal, wait::WaitStatus},
    unistd::{Pid, Uid},
};
use snafu::Report;
use tokio_util::sync::CancellationToken;

use super::{
    CleanupSummary, FailedWorkerRecord, RootState, WorkerFailure, WorkerProcessError,
    WorkerProcessRecord, owner::Owner,
};
use crate::hypervisor::{task_scope::TaskScope, worker_handle::WorkerHandle};

impl RootState {
    // -----------------------------------------------------------------------
    // In-process task/resource scope
    // -----------------------------------------------------------------------

    pub fn local_task_scope(&self) -> TaskScope {
        self.local_tasks.clone()
    }

    /// Spawn and track a root-side background task owned by an in-process service.
    pub fn spawn_local_task<F, Fut>(&self, task: F)
    where
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        self.local_tasks.spawn(task);
    }

    /// Cancel in-process background tasks and retire in-process listeners.
    pub async fn cleanup_local_resources(self: &Arc<Self>) -> usize {
        let owner = Owner::Local;
        let local_listeners = {
            let registry = self.listeners.read().await;
            registry.owned_names(owner)
        };
        let cleaned = local_listeners.len();
        for server_name in &local_listeners {
            if let Err(error) = self.release_listener(owner, server_name).await {
                tracing::warn!(
                    %server_name,
                    error = %Report::from_error(&error),
                    "failed to release local listener"
                );
            }
        }

        self.local_tasks.cancel_and_wait().await;

        cleaned
    }

    // -----------------------------------------------------------------------
    // Worker registry
    // -----------------------------------------------------------------------

    /// Register a new worker process.
    ///
    /// If another worker already holds the same UID, the old one is cleaned
    /// up first (uid-replaced).
    pub async fn register_worker_with_defaults(
        self: &Arc<Self>,
        pid: Pid,
        uid: Uid,
        username: String,
        worker_handle: WorkerHandle,
        root_defaults_tx: remoc::rch::watch::Sender<
            gateway::parse::config::RootWorkerDefaultsSnapshot,
            remoc::codec::Default,
        >,
    ) {
        let replaced_pid = {
            let inner = self.inner.lock().await;
            inner
                .users
                .get(&uid)
                .copied()
                .filter(|old_pid| *old_pid != pid)
        };

        if let Some(old_pid) = replaced_pid {
            let _ = self
                .cleanup_worker(old_pid, WorkerProcessError::UidReplaced)
                .await;
        }

        let mut inner = self.inner.lock().await;
        inner.processes.insert(
            pid,
            WorkerProcessRecord {
                uid,
                username: username.clone(),
                tasks: TaskScope::new(),
                worker_handle,
                root_defaults_tx,
            },
        );
        inner.users.insert(uid, pid);
        tracing::debug!(pid = %pid, uid = uid.as_raw(), %username, "registered worker");
    }

    pub async fn dispatch_worker_defaults(
        &self,
        snapshot: gateway::parse::config::RootWorkerDefaultsSnapshot,
    ) {
        let inner = self.inner.lock().await;
        for (pid, record) in &inner.processes {
            if let Err(error) = record.root_defaults_tx.send(snapshot.clone()) {
                tracing::warn!(pid = %pid, error = %error, "failed to dispatch worker defaults snapshot");
            }
        }
    }

    #[cfg(test)]
    pub async fn register_worker(
        self: &Arc<Self>,
        pid: Pid,
        uid: Uid,
        username: String,
        worker_handle: WorkerHandle,
    ) {
        let defaults = gateway::parse::TypedConfigParser::new()
            .parse_root(
                "pishoo {}",
                std::path::Path::new("/tmp/test-root.conf"),
                None,
            )
            .unwrap()
            .pishoo()
            .worker_defaults();
        let (root_defaults_tx, _root_defaults_rx) = remoc::rch::watch::channel(defaults);
        self.register_worker_with_defaults(pid, uid, username, worker_handle, root_defaults_tx)
            .await;
    }

    /// Check whether a worker with the given PID is registered.
    pub async fn has_worker(&self, pid: Pid) -> bool {
        self.inner.lock().await.processes.contains_key(&pid)
    }

    /// Spawn and track a root-side background task for a worker.
    ///
    /// If the worker is no longer registered, the task is not spawned.
    pub async fn spawn_worker_task<F, Fut>(&self, pid: Pid, task: F) -> bool
    where
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let scope = {
            let inner = self.inner.lock().await;
            inner
                .processes
                .get(&pid)
                .map(|process| process.tasks.clone())
        };
        if let Some(scope) = scope {
            scope.spawn(task);
            true
        } else {
            false
        }
    }

    /// Remove all resources for a dead/exited worker process.
    ///
    /// Acquires `inner` lock first to remove process bookkeeping, then
    /// releases listener resources through the listener registry.
    pub async fn cleanup_worker(
        self: &Arc<Self>,
        pid: Pid,
        error: WorkerProcessError,
    ) -> Option<CleanupSummary> {
        let report = Report::from_error(&error);
        // Step 1: remove process record.
        let restartable = error.is_restartable();
        let (record_uid, record_username, tasks) = {
            let mut inner = self.inner.lock().await;
            let record = inner.processes.remove(&pid)?;

            // Only remove uid→pid mapping if it still points to this pid.
            if inner.users.get(&record.uid).copied() == Some(pid) {
                inner.users.remove(&record.uid);
            }

            // If the worker is still desired and failed in a restartable way,
            // park its target into `failed_workers` so the next reload can
            // retry it. Non-restartable failures (e.g. reload-driven removal)
            // are not parked.
            if restartable && let Some(target) = inner.desired_workers.get(&record.uid).cloned() {
                inner.failed_workers.insert(
                    record.uid,
                    FailedWorkerRecord {
                        target,
                        reason: report.to_string(),
                    },
                );
            }

            (record.uid, record.username, record.tasks)
        };
        // inner lock released here.

        let owner = Owner::worker(record_uid, pid);
        let owned_listeners = {
            let registry = self.listeners.read().await;
            registry.owned_names(owner)
        };

        let mut servers_cleaned = 0usize;
        for server_name in &owned_listeners {
            match self.release_listener(owner, server_name).await {
                Ok(()) => servers_cleaned += 1,
                Err(error) => tracing::warn!(
                    %server_name,
                    pid = %pid,
                    error = %Report::from_error(&error),
                    "failed to release worker listener"
                ),
            }
        }

        // Step 2: request all scoped worker tasks to shut down and wait for
        // them to finish after listener resources have been explicitly retired.
        let background_tasks_cleaned = tasks.len();
        tasks.cancel_and_wait().await;

        let summary = CleanupSummary {
            pid,
            uid: record_uid,
            servers_cleaned,
            background_tasks_cleaned,
        };
        tracing::info!(
            pid = %summary.pid,
            username = %record_username,
            servers_cleaned = summary.servers_cleaned,
            error = %report,
            "worker stopped"
        );

        Some(summary)
    }

    pub async fn retire_owner(self: &Arc<Self>, owner: Owner) -> usize {
        let owned_listeners = {
            let registry = self.listeners.read().await;
            registry.owned_names(owner)
        };
        let mut cleaned = 0usize;
        for server_name in &owned_listeners {
            if self.release_listener(owner, server_name).await.is_ok() {
                cleaned += 1;
            }
        }
        cleaned
    }

    pub async fn fail_worker(&self, pid: Pid, error: WorkerProcessError) {
        let mut inner = self.inner.lock().await;
        if !inner.processes.contains_key(&pid) {
            return;
        }
        inner
            .worker_failures
            .push_back(WorkerFailure { pid, error });
        drop(inner);
        self.worker_notify.notify_one();
    }

    /// Collect worker failures reported by IPC tasks and exited processes.
    pub async fn collect_worker_failures(&self) -> Vec<WorkerFailure> {
        let mut inner = self.inner.lock().await;
        let mut failures = inner.worker_failures.drain(..).collect::<Vec<_>>();
        let queued_pids = failures
            .iter()
            .map(|failure| failure.pid)
            .collect::<HashSet<_>>();
        for (pid, process) in &mut inner.processes {
            if queued_pids.contains(pid) {
                continue;
            }
            match process.worker_handle.try_wait() {
                Ok(Some(status)) => match status {
                    WaitStatus::StillAlive => {}
                    _ => {
                        tracing::warn!(pid = %pid, ?status, "worker exited");
                        failures.push(WorkerFailure {
                            pid: *pid,
                            error: WorkerProcessError::ChildExited { status },
                        });
                    }
                },
                Ok(None) => {}
                Err(error) => {
                    tracing::error!(
                        pid = %pid,
                        error = %Report::from_error(&error),
                        "failed to poll worker status"
                    );
                    failures.push(WorkerFailure {
                        pid: *pid,
                        error: WorkerProcessError::PollStatus { source: error },
                    });
                }
            }
        }
        failures
    }

    /// Get all registered worker PIDs.
    pub async fn worker_pids(&self) -> Vec<Pid> {
        self.inner.lock().await.processes.keys().copied().collect()
    }

    /// Send SIGKILL to all registered workers.
    pub async fn force_kill_workers(&self, cause: &WorkerProcessError) -> Vec<Pid> {
        let mut inner = self.inner.lock().await;
        let mut killed = Vec::new();
        for (pid, process) in &mut inner.processes {
            match process.worker_handle.start_kill() {
                Ok(()) => {
                    tracing::warn!(
                        pid = %pid,
                        cause = %Report::from_error(cause),
                        "sent SIGKILL to worker"
                    );
                    killed.push(*pid);
                }
                Err(error) => {
                    tracing::warn!(
                        pid = %pid,
                        cause = %Report::from_error(cause),
                        error = %Report::from_error(&error),
                        "failed to force kill worker"
                    );
                }
            }
        }
        killed
    }

    /// Forward a Unix signal to all registered workers.
    pub async fn forward_unix_signal(&self, signal: Signal) {
        let inner = self.inner.lock().await;
        for (pid, record) in &inner.processes {
            let child_pid = record.worker_handle.pid();
            if let Err(error) = nix::sys::signal::kill(child_pid, signal) {
                tracing::warn!(
                    pid = %pid,
                    error = %Report::from_error(&error),
                    ?signal,
                    "failed to forward unix signal to worker"
                );
            }
        }
    }

    /// Send a Unix signal to a specific worker by UID.
    pub async fn send_signal_to_user(&self, uid: Uid, signal: Signal) {
        let inner = self.inner.lock().await;
        if let Some(&pid) = inner.users.get(&uid)
            && let Some(record) = inner.processes.get(&pid)
        {
            let child_pid = record.worker_handle.pid();
            if let Err(error) = nix::sys::signal::kill(child_pid, signal) {
                tracing::warn!(
                    pid = %pid,
                    uid = uid.as_raw(),
                    error = %Report::from_error(&error),
                    ?signal,
                    "failed to send signal to worker"
                );
            }
        }
    }

    /// Wait for a worker to exit with a timeout.
    ///
    /// Returns `true` if the worker exited before the deadline.
    pub async fn wait_worker_exit(&self, pid: Pid, timeout: std::time::Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            {
                let mut inner = self.inner.lock().await;
                if !inner.processes.contains_key(&pid) {
                    return true;
                }
                if let Some(record) = inner.processes.get_mut(&pid) {
                    match record.worker_handle.try_wait() {
                        Ok(Some(WaitStatus::StillAlive)) | Ok(None) => {}
                        Ok(Some(_)) => return true,
                        Err(_) => return true,
                    }
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// Get the PID for a worker running under the given UID, if any.
    pub async fn pid_for_uid(&self, uid: Uid) -> Option<Pid> {
        self.inner.lock().await.users.get(&uid).copied()
    }

    /// Send SIGKILL to a specific worker by PID.
    pub async fn force_kill_worker(&self, pid: Pid, cause: &WorkerProcessError) {
        let mut inner = self.inner.lock().await;
        if let Some(record) = inner.processes.get_mut(&pid) {
            if let Err(error) = record.worker_handle.start_kill() {
                tracing::warn!(
                    pid = %pid,
                    cause = %Report::from_error(cause),
                    error = %Report::from_error(&error),
                    "failed to force kill worker"
                );
            } else {
                tracing::warn!(
                    pid = %pid,
                    cause = %Report::from_error(cause),
                    "sent SIGKILL to worker"
                );
            }
        }
    }

    pub async fn set_desired_workers(&self, targets: Vec<crate::config::WorkerAccount>) {
        let mut inner = self.inner.lock().await;
        let desired: std::collections::HashMap<_, _> = targets
            .into_iter()
            .map(|target| (target.uid(), target))
            .collect();
        inner
            .failed_workers
            .retain(|uid, _| desired.contains_key(uid));
        inner.desired_workers = desired;
    }

    /// Drain the set of failed-but-still-desired workers so the reload
    /// orchestrator can retry spawning them. After draining, the registry
    /// is empty until the next worker failure parks another entry.
    pub async fn take_failed_desired_workers(&self) -> Vec<crate::config::WorkerAccount> {
        let mut inner = self.inner.lock().await;
        inner
            .failed_workers
            .drain()
            .map(|(uid, record)| {
                tracing::info!(
                    uid = uid.as_raw(),
                    user = %record.target.name(),
                    reason = %record.reason,
                    "retrying failed worker on reload"
                );
                record.target
            })
            .collect()
    }

    pub async fn clear_desired_workers(&self) {
        self.inner.lock().await.desired_workers.clear();
    }

    pub async fn desired_worker_target(&self, uid: Uid) -> Option<crate::config::WorkerAccount> {
        self.inner.lock().await.desired_workers.get(&uid).cloned()
    }
}
