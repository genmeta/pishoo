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

use gateway::control_plane::ListenRequest;
use nix::{
    sys::{signal::Signal, wait::WaitStatus},
    unistd::{Pid, Uid},
};
use snafu::{Report, ResultExt, Snafu};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

use crate::{listen::PerServerListener, root::worker_handle::WorkerHandle};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Error returned when `register_listener` fails.
#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum RegisterError {
    /// The name is already owned by the same owner — duplicate listen attempt.
    #[snafu(display("duplicate listen for the same owner"))]
    DuplicateListen,
    /// The name is owned by a different owner, or was already poisoned.
    /// The entry has been poisoned (set to `Conflicted`).
    #[snafu(display("server name conflicted (poisoned)"))]
    ConflictedName,
    /// Failed to bind the server in [`QuicListeners`].
    #[snafu(display("failed to add server `{server_name}` to listeners"))]
    AddServerFailed {
        server_name: String,
        source: gm_quic::prelude::ServerError,
    },
}

/// Identifies the owner of a server_name registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceOwner {
    /// Owned by the root-local service.
    Local,
    /// Owned by a specific worker process.
    Worker(Pid),
}

/// Per-server ownership record stored in the root registry.
pub enum ServerEntry {
    /// Name is actively owned and serving.
    Active {
        owner: ServiceOwner,
        conn_tx: mpsc::Sender<gm_quic::prelude::Connection>,
        shutdown_token: CancellationToken,
    },
    /// Name is poisoned due to a cross-owner conflict.
    /// Only cleared by `scrub_conflicts()` during reload.
    Conflicted,
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
    /// Per-server_name async gates for serializing register_listener calls.
    name_gates: HashMap<String, Arc<Mutex<()>>>,
}

impl Inner {
    /// Remove a server entry, cancel its listener, and remove from QuicListeners.
    /// For `Conflicted` entries, only removes the map entry (no listener to cancel).
    fn retire_server(
        &mut self,
        server_name: &str,
        listeners: &gm_quic::prelude::QuicListeners,
    ) -> Option<()> {
        let entry = self.servers.remove(server_name)?;
        match entry {
            ServerEntry::Active { shutdown_token, .. } => {
                shutdown_token.cancel();
                listeners.remove_server(server_name);
            }
            ServerEntry::Conflicted => {
                // Already poisoned — no listener to cancel, QuicListeners
                // already had the server removed when the conflict was created.
            }
        }
        Some(())
    }

    /// Full cleanup of a worker process, removing all its resources.
    ///
    /// Only retires `Active` entries that are still owned by this worker.
    /// `Conflicted` entries are left in place — only `scrub_conflicts()`
    /// during reload can clear them.
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
            // Only retire if the entry is Active and still owned by this worker.
            let dominated = matches!(
                self.servers.get(server_name.as_str()),
                Some(ServerEntry::Active { owner: ServiceOwner::Worker(p), .. }) if *p == pid
            );
            if dominated && self.retire_server(server_name, listeners).is_some() {
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
                name_gates: HashMap::new(),
            }),
        }
    }

    // -----------------------------------------------------------------------
    // Server registry
    // -----------------------------------------------------------------------

    /// Get or create the per-name async gate for serializing operations on a
    /// single `server_name`. The gate is held across the async `add_server`
    /// call so that concurrent register_listener for the same name are
    /// properly serialized.
    async fn name_gate(&self, server_name: &str) -> Arc<Mutex<()>> {
        let mut inner = self.inner.lock().await;
        inner
            .name_gates
            .entry(server_name.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Unified entry point for registering a listener.
    ///
    /// Semantics:
    /// - **Vacant**: bind via `add_server`, commit as `Active`.
    /// - **Active, same owner**: return `DuplicateListen` (no state change).
    /// - **Active, different owner**: poison the name to `Conflicted`,
    ///   cancel the existing listener, remove from QuicListeners, return
    ///   `ConflictedName`.
    /// - **Conflicted**: return `ConflictedName` (no state change).
    ///
    /// Returns a [`PerServerListener`] on success.
    pub async fn register_listener(
        &self,
        owner: ServiceOwner,
        request: ListenRequest,
    ) -> Result<PerServerListener, RegisterError> {
        let server_name = request.identity.name().as_full().to_owned();

        // Acquire the per-name gate so that concurrent calls for the same
        // server_name are serialized. This is critical because `add_server`
        // is async and we must not race.
        let gate = self.name_gate(&server_name).await;
        let _guard = gate.lock().await;

        // Phase 1: check current state and handle conflicts (under inner lock).
        {
            let mut inner = self.inner.lock().await;
            match inner.servers.get(&server_name) {
                Some(ServerEntry::Active {
                    owner: existing_owner,
                    ..
                }) => {
                    if *existing_owner == owner {
                        return Err(RegisterError::DuplicateListen);
                    }
                    // Cross-owner conflict — poison the name immediately.
                    let old = inner
                        .servers
                        .insert(server_name.clone(), ServerEntry::Conflicted);
                    if let Some(ServerEntry::Active {
                        shutdown_token,
                        owner: old_owner,
                        ..
                    }) = old
                    {
                        shutdown_token.cancel();
                        self.listeners.remove_server(&server_name);
                        // Remove from old owner's owned_servers.
                        if let ServiceOwner::Worker(pid) = old_owner
                            && let Some(proc) = inner.processes.get_mut(&pid)
                        {
                            proc.owned_servers.remove(&server_name);
                        }
                    }
                    tracing::warn!(
                        %server_name,
                        new_owner = ?owner,
                        "Cross-owner conflict: name poisoned"
                    );
                    return Err(RegisterError::ConflictedName);
                }
                Some(ServerEntry::Conflicted) => {
                    return Err(RegisterError::ConflictedName);
                }
                None => {
                    // Vacant — fall through to bind + commit.
                }
            }
        }

        // Phase 2: name is vacant — bind the server (slow, async I/O).
        self.listeners
            .add_server(
                &server_name,
                request.identity.certs(),
                request.identity.key(),
                request.bind,
                None::<Vec<u8>>,
            )
            .await
            .context(register_error::AddServerFailedSnafu {
                server_name: &server_name,
            })?;

        // Phase 3: commit registration (reacquire inner lock).
        let mut inner = self.inner.lock().await;

        // Defensive re-check: someone may have raced despite the per-name
        // gate (shouldn't happen, but belt-and-suspenders).
        if inner.servers.contains_key(&server_name) {
            self.listeners.remove_server(&server_name);
            tracing::error!(
                %server_name,
                "BUG: server appeared in registry after add_server despite name gate"
            );
            inner
                .servers
                .insert(server_name.clone(), ServerEntry::Conflicted);
            return Err(RegisterError::ConflictedName);
        }

        let (tx, rx) = mpsc::channel(128);
        let shutdown_token = CancellationToken::new();

        // Track in the worker's owned_servers set.
        if let ServiceOwner::Worker(pid) = owner
            && let Some(process) = inner.processes.get_mut(&pid)
        {
            process.owned_servers.insert(server_name.clone());
        }

        inner.servers.insert(
            server_name.clone(),
            ServerEntry::Active {
                owner,
                conn_tx: tx,
                shutdown_token: shutdown_token.clone(),
            },
        );

        tracing::info!(%server_name, ?owner, "server registered");
        Ok(PerServerListener::new(rx, shutdown_token))
    }

    /// Look up the routing sender for a given server_name.
    ///
    /// Used by the central accept loop to route connections to the correct
    /// per-server adapter. Returns `None` for `Conflicted` entries.
    pub async fn get_conn_sender(
        &self,
        server_name: &str,
    ) -> Option<mpsc::Sender<gm_quic::prelude::Connection>> {
        let inner = self.inner.lock().await;
        match inner.servers.get(server_name) {
            Some(ServerEntry::Active { conn_tx, .. }) => Some(conn_tx.clone()),
            _ => None,
        }
    }

    /// Retire all servers owned by the root-local service.
    ///
    /// Returns the list of retired server_names.
    pub async fn retire_local_servers(&self) -> Vec<String> {
        let mut inner = self.inner.lock().await;
        let local_names: Vec<String> = inner
            .servers
            .iter()
            .filter_map(|(name, entry)| match entry {
                ServerEntry::Active {
                    owner: ServiceOwner::Local,
                    ..
                } => Some(name.clone()),
                _ => None,
            })
            .collect();

        for name in &local_names {
            inner.retire_server(name, &self.listeners);
        }

        local_names
    }

    /// Remove all `Conflicted` entries from the registry.
    ///
    /// Called during reload (SIGHUP) **before** forwarding the signal to
    /// workers, so that workers can re-register previously-conflicted names.
    pub async fn scrub_conflicts(&self) -> Vec<String> {
        let mut inner = self.inner.lock().await;
        let conflicted: Vec<String> = inner
            .servers
            .iter()
            .filter_map(|(name, entry)| {
                matches!(entry, ServerEntry::Conflicted).then_some(name.clone())
            })
            .collect();

        for name in &conflicted {
            inner.servers.remove(name);
            inner.name_gates.remove(name);
        }

        if !conflicted.is_empty() {
            tracing::info!(
                count = conflicted.len(),
                names = ?conflicted,
                "scrubbed conflicted server entries during reload"
            );
        }

        conflicted
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
        if let Some(&old_pid) = inner.users.get(&uid)
            && old_pid != pid
        {
            inner.cleanup_worker(old_pid, "uid_replaced", &self.listeners);
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
            let child_pid = record.worker_handle.pid();
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
