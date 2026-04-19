//! Worker/process registry operations on [`RootState`].

use std::collections::HashSet;

use nix::{
    sys::{signal::Signal, wait::WaitStatus},
    unistd::{Pid, Uid},
};
use snafu::Report;
use tokio::task::JoinSet;

use super::{CleanupSummary, RootState, ServerEntry, ServiceOwner, WorkerProcessRecord};
use crate::hypervisor::worker_handle::WorkerHandle;

impl RootState {
    // -----------------------------------------------------------------------
    // Worker registry
    // -----------------------------------------------------------------------

    /// Register a new worker process.
    ///
    /// If another worker already holds the same UID, the old one is cleaned
    /// up first (uid-replaced).
    pub async fn register_worker(
        &self,
        pid: Pid,
        uid: Uid,
        username: String,
        worker_handle: WorkerHandle,
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
                .cleanup_worker_with_reason(old_pid, "uid_replaced")
                .await;
        }

        let mut inner = self.inner.lock().await;
        inner.processes.insert(
            pid,
            WorkerProcessRecord {
                uid,
                username: username.clone(),
                owned_servers: HashSet::new(),
                worker_handle,
            },
        );
        inner.users.insert(uid, pid);
        tracing::debug!(pid = %pid, uid = uid.as_raw(), %username, "registered worker");
    }

    /// Check whether a worker with the given PID is registered.
    pub async fn has_worker(&self, pid: Pid) -> bool {
        self.inner.lock().await.processes.contains_key(&pid)
    }

    /// Spawn and track a root-side background task for a worker.
    ///
    /// If the worker is no longer registered, the task is not spawned.
    pub async fn spawn_worker_task<F>(&self, pid: Pid, task: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let mut inner = self.inner.lock().await;
        if !inner.processes.contains_key(&pid) {
            return;
        }
        inner
            .worker_tasks
            .entry(pid)
            .or_insert_with(JoinSet::new)
            .spawn(task);
    }

    /// Remove all resources for a dead/exited worker process.
    ///
    /// Acquires `inner` lock first (to collect owned servers), releases it,
    /// then acquires `servers` write lock for cleanup. The two locks are
    /// never held simultaneously.
    pub async fn cleanup_worker_with_reason(
        &self,
        pid: Pid,
        reason: &str,
    ) -> Option<CleanupSummary> {
        // Step 1: remove process record and collect owned server names.
        let (record_uid, record_username, owned_servers, background_tasks) = {
            let mut inner = self.inner.lock().await;
            let record = inner.processes.remove(&pid)?;

            // Only remove uid→pid mapping if it still points to this pid.
            if inner.users.get(&record.uid).copied() == Some(pid) {
                inner.users.remove(&record.uid);
            }

            let background_tasks = inner.worker_tasks.remove(&pid).unwrap_or_default();
            (
                record.uid,
                record.username,
                record.owned_servers,
                background_tasks,
            )
        };
        // inner lock released here.

        // Step 2: retire owned servers under the servers write lock.
        // Also scan for `Registering` entries owned by this worker — they are
        // not yet recorded in `owned_servers` because `register_listener`
        // Phase 3 has not completed.
        let (servers_cleaned, retired_tasks) = {
            let mut registry = self.servers.write().await;
            let mut cleaned = 0usize;
            let mut retired: Vec<tokio_util::task::AbortOnDropHandle<()>> = Vec::new();
            for server_name in &owned_servers {
                let dominated = matches!(
                    registry.entries.get(server_name.as_str()),
                    Some(ServerEntry::Active { owner: ServiceOwner::Worker(p), .. }) if *p == pid
                );
                if dominated {
                    if let Some(task) = registry.retire_entry(server_name) {
                        retired.push(task);
                    }
                    cleaned += 1;
                }
            }
            // Full-scan for Registering sentinels owned by the dead worker.
            let orphaned: Vec<String> = registry
                .entries
                .iter()
                .filter(|&(_, entry)| {
                    matches!(
                        entry,
                        ServerEntry::Registering { owner: ServiceOwner::Worker(p) } if *p == pid
                    )
                })
                .map(|(name, _)| name.clone())
                .collect();
            for name in &orphaned {
                if let Some(task) = registry.retire_entry(name) {
                    retired.push(task);
                }
                cleaned += 1;
            }
            (cleaned, retired)
        };
        // servers lock released here — await the retired fanout tasks so
        // their captured `ServerBinding`s are dropped before we return.
        for task in retired_tasks {
            let _ = task.await;
        }

        let background_tasks_cleaned = background_tasks.len();
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
            %reason,
            "worker stopped"
        );

        // Step 3: drain background tasks (no lock held).
        let mut tasks = background_tasks;
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}

        Some(summary)
    }

    /// Collect PIDs of workers whose processes have exited.
    pub async fn collect_exited_workers(&self) -> Vec<Pid> {
        let mut inner = self.inner.lock().await;
        let mut exited = Vec::new();
        for (pid, process) in &mut inner.processes {
            match process.worker_handle.try_wait() {
                Ok(Some(status)) => match status {
                    WaitStatus::StillAlive => {}
                    _ => {
                        tracing::warn!(pid = %pid, ?status, "worker exited");
                        exited.push(*pid);
                    }
                },
                Ok(None) => {}
                Err(error) => {
                    tracing::error!(
                        pid = %pid,
                        error = %Report::from_error(&error),
                        "failed to poll worker status"
                    );
                    exited.push(*pid);
                }
            }
        }
        exited
    }

    /// Get all registered worker PIDs.
    pub async fn worker_pids(&self) -> Vec<Pid> {
        self.inner.lock().await.processes.keys().copied().collect()
    }

    /// Send SIGKILL to all registered workers.
    pub async fn force_kill_workers(&self, reason: &str) -> Vec<Pid> {
        let mut inner = self.inner.lock().await;
        let mut killed = Vec::new();
        for (pid, process) in &mut inner.processes {
            match process.worker_handle.start_kill() {
                Ok(()) => {
                    tracing::warn!(pid = %pid, %reason, "sent SIGKILL to worker");
                    killed.push(*pid);
                }
                Err(error) => {
                    tracing::warn!(
                        pid = %pid,
                        %reason,
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
    pub async fn force_kill_worker(&self, pid: Pid) {
        let mut inner = self.inner.lock().await;
        if let Some(record) = inner.processes.get_mut(&pid) {
            if let Err(error) = record.worker_handle.start_kill() {
                tracing::warn!(
                    pid = %pid,
                    error = %Report::from_error(&error),
                    "failed to force kill worker"
                );
            } else {
                tracing::warn!(pid = %pid, "sent SIGKILL to worker");
            }
        }
    }
}
