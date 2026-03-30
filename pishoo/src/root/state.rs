//! Root-side ownership registry for server_name → local/worker mappings.
//!
//! Tracks which worker process owns which server names, provides conflict
//! detection, and manages the lifecycle of per-server listen adapters routed
//! from the central [`QuicListeners`].
//!
//! All mutating methods take `&self` and use interior mutability so that
//! `RootState` can be shared via `Arc` without external synchronization.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use nix::{
    sys::{signal::Signal, wait::WaitStatus},
    unistd::{Pid, Uid},
};
use snafu::Report;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

use crate::worker_spawn::WorkerHandle;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Error returned when server registration fails due to a name conflict.
#[derive(Debug)]
pub struct RegisterConflict;

impl std::fmt::Display for RegisterConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "server name already registered")
    }
}

impl std::error::Error for RegisterConflict {}

/// Identifies the owner of a server_name registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceOwner {
    /// Owned by the root-local service.
    Local,
    /// Owned by a specific worker process.
    Worker(Pid),
}

/// Per-server ownership record stored in the root registry.
pub struct ServerEntry {
    /// Owner of this server_name.
    pub owner: ServiceOwner,
    /// Sender for routing connections from the central accept loop.
    pub conn_tx: mpsc::Sender<gm_quic::prelude::Connection>,
    /// Shutdown token for the associated [`PerServerListener`](crate::per_server_listen::PerServerListener).
    pub shutdown_token: CancellationToken,
}

/// Per-worker-process tracking record.
struct WorkerProcessRecord {
    /// The UID this worker runs as.
    uid: Uid,
    /// Set of server_names owned by this worker.
    owned_servers: HashSet<String>,
    /// Handle to the spawned worker process.
    worker_handle: WorkerHandle,
    /// Cancellation tokens for connector serve futures owned by this worker.
    connector_shutdown_tokens: Vec<CancellationToken>,
}

/// Summary produced by worker cleanup.
#[derive(Debug, Clone)]
pub struct CleanupSummary {
    pub pid: Pid,
    pub uid: Uid,
    pub servers_cleaned: usize,
    pub connectors_cleaned: usize,
}

// ---------------------------------------------------------------------------
// Inner state (behind Mutex)
// ---------------------------------------------------------------------------

struct Inner {
    /// server_name → ownership + routing sender.
    servers: HashMap<String, ServerEntry>,
    /// pid → worker process info.
    processes: HashMap<Pid, WorkerProcessRecord>,
    /// uid → pid mapping (one worker per uid).
    users: HashMap<Uid, Pid>,
}

impl Inner {
    /// Remove a server entry, cancel its listener, and remove from QuicListeners.
    fn retire_server(
        &mut self,
        server_name: &str,
        listeners: &gm_quic::prelude::QuicListeners,
    ) -> Option<()> {
        let record = self.servers.remove(server_name)?;
        record.shutdown_token.cancel();
        listeners.remove_server(server_name);
        Some(())
    }

    /// Full cleanup of a worker process, removing all its resources.
    fn cleanup_worker(
        &mut self,
        pid: Pid,
        reason: &str,
        listeners: &gm_quic::prelude::QuicListeners,
    ) -> Option<CleanupSummary> {
        let record = self.processes.remove(&pid)?;

        // Only remove uid→pid mapping if it still points to this pid.
        if self.users.get(&record.uid).copied() == Some(pid) {
            self.users.remove(&record.uid);
        }

        let mut servers_cleaned = 0usize;
        for server_name in &record.owned_servers {
            if self.retire_server(server_name, listeners).is_some() {
                servers_cleaned += 1;
            }
        }

        let connectors_cleaned = record.connector_shutdown_tokens.len();
        for token in &record.connector_shutdown_tokens {
            token.cancel();
        }

        let summary = CleanupSummary {
            pid,
            uid: record.uid,
            servers_cleaned,
            connectors_cleaned,
        };
        tracing::info!(
            pid = %summary.pid,
            uid = summary.uid.as_raw(),
            servers_cleaned = summary.servers_cleaned,
            connectors_cleaned = summary.connectors_cleaned,
            %reason,
            "Worker cleanup complete"
        );
        Some(summary)
    }
}

// ---------------------------------------------------------------------------
// RootState
// ---------------------------------------------------------------------------

/// Root-side ownership registry (thread-safe, interior mutability).
///
/// Tracks `server_name → owner`, `pid → owned_servers`, and `uid → pid`
/// mappings. Owns the shared [`QuicListeners`] and coordinates all
/// server registration / cleanup.
pub struct RootState {
    /// The shared QUIC listeners object.
    pub listeners: Arc<gm_quic::prelude::QuicListeners>,
    inner: Mutex<Inner>,
}

impl RootState {
    /// Create a new root state with the given shared QUIC listeners.
    pub fn new(listeners: Arc<gm_quic::prelude::QuicListeners>) -> Self {
        Self {
            listeners,
            inner: Mutex::new(Inner {
                servers: HashMap::new(),
                processes: HashMap::new(),
                users: HashMap::new(),
            }),
        }
    }

    // -----------------------------------------------------------------------
    // Server registry
    // -----------------------------------------------------------------------

    /// Check whether a server_name is already registered.
    pub async fn has_server(&self, server_name: &str) -> bool {
        self.inner.lock().await.servers.contains_key(server_name)
    }

    /// Register a server_name with the given entry.
    ///
    /// Fails if the server_name is already taken. On success, records the
    /// server in the owning worker's `owned_servers` set (if applicable).
    pub async fn register_server(
        &self,
        server_name: String,
        entry: ServerEntry,
    ) -> Result<(), RegisterConflict> {
        let mut inner = self.inner.lock().await;
        if inner.servers.contains_key(&server_name) {
            return Err(RegisterConflict);
        }

        // Track in the worker's owned_servers set.
        if let ServiceOwner::Worker(pid) = entry.owner {
            if let Some(process) = inner.processes.get_mut(&pid) {
                process.owned_servers.insert(server_name.clone());
            }
        }

        inner.servers.insert(server_name, entry);
        Ok(())
    }

    /// Look up the routing sender for a given server_name.
    ///
    /// Used by the central accept loop to route connections to the correct
    /// per-server adapter.
    pub async fn get_conn_sender(
        &self,
        server_name: &str,
    ) -> Option<mpsc::Sender<gm_quic::prelude::Connection>> {
        self.inner
            .lock()
            .await
            .servers
            .get(server_name)
            .map(|r| r.conn_tx.clone())
    }

    /// Retire all servers owned by the root-local service.
    ///
    /// Returns the list of retired server_names.
    pub async fn retire_local_servers(&self) -> Vec<String> {
        let mut inner = self.inner.lock().await;
        let local_names: Vec<String> = inner
            .servers
            .iter()
            .filter_map(|(name, entry)| {
                (entry.owner == ServiceOwner::Local).then_some(name.clone())
            })
            .collect();

        for name in &local_names {
            inner.retire_server(name, &self.listeners);
        }

        local_names
    }

    // -----------------------------------------------------------------------
    // Worker registry
    // -----------------------------------------------------------------------

    /// Register a new worker process.
    ///
    /// If another worker already holds the same UID, the old one is cleaned
    /// up first (uid-replaced).
    pub async fn register_worker(&self, pid: Pid, uid: Uid, worker_handle: WorkerHandle) {
        let mut inner = self.inner.lock().await;

        // If the same uid is already held by a different pid, clean up the old one.
        if let Some(&old_pid) = inner.users.get(&uid) {
            if old_pid != pid {
                inner.cleanup_worker(old_pid, "uid_replaced", &self.listeners);
            }
        }

        inner.processes.insert(
            pid,
            WorkerProcessRecord {
                uid,
                owned_servers: HashSet::new(),
                worker_handle,
                connector_shutdown_tokens: Vec::new(),
            },
        );
        inner.users.insert(uid, pid);
        tracing::info!(pid = %pid, uid = uid.as_raw(), "Registered worker");
    }

    /// Check whether a worker with the given PID is registered.
    pub async fn has_worker(&self, pid: Pid) -> bool {
        self.inner.lock().await.processes.contains_key(&pid)
    }

    /// Track a connector cancellation token for a worker.
    pub async fn add_connector_token(&self, pid: Pid, token: CancellationToken) {
        let mut inner = self.inner.lock().await;
        if let Some(process) = inner.processes.get_mut(&pid) {
            process.connector_shutdown_tokens.push(token);
        }
    }

    /// Remove all resources for a dead/exited worker process.
    pub async fn cleanup_worker_with_reason(
        &self,
        pid: Pid,
        reason: &str,
    ) -> Option<CleanupSummary> {
        self.inner
            .lock()
            .await
            .cleanup_worker(pid, reason, &self.listeners)
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
                        tracing::warn!(pid = %pid, ?status, "Worker exited");
                        exited.push(*pid);
                    }
                },
                Ok(None) => {}
                Err(error) => {
                    tracing::error!(
                        pid = %pid,
                        error = %Report::from_error(&error),
                        "Failed to poll worker status"
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
                    tracing::warn!(pid = %pid, %reason, "Sent SIGKILL to worker");
                    killed.push(*pid);
                }
                Err(error) => {
                    tracing::warn!(
                        pid = %pid,
                        %reason,
                        error = %Report::from_error(&error),
                        "Failed to force kill worker"
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
            let Some(raw_pid) = record.worker_handle.pid() else {
                continue;
            };
            let child_pid = Pid::from_raw(raw_pid as i32);
            if let Err(error) = nix::sys::signal::kill(child_pid, signal) {
                tracing::warn!(
                    pid = %pid,
                    error = %Report::from_error(&error),
                    ?signal,
                    "Failed to forward unix signal to worker"
                );
            }
        }
    }
}
