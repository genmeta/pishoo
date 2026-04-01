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
use tokio::{
    sync::{Mutex, mpsc},
    task::JoinSet,
};
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
        /// Original listen specifications for network-change reconciliation.
        listens: Vec<gateway::parse::Listens>,
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
}

struct WorkerCleanupArtifacts {
    summary: CleanupSummary,
    background_tasks: JoinSet<()>,
}

/// Summary produced by worker cleanup.
#[derive(Debug, Clone)]
pub struct CleanupSummary {
    pub pid: Pid,
    pub uid: Uid,
    pub servers_cleaned: usize,
    pub background_tasks_cleaned: usize,
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
    /// Root-side background tasks grouped by worker pid.
    worker_tasks: HashMap<Pid, JoinSet<()>>,
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
        self.name_gates.remove(server_name);
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
    ) -> Option<WorkerCleanupArtifacts> {
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

        let background_tasks = self.worker_tasks.remove(&pid).unwrap_or_default();
        let background_tasks_cleaned = background_tasks.len();

        let summary = CleanupSummary {
            pid,
            uid: record.uid,
            servers_cleaned,
            background_tasks_cleaned,
        };
        tracing::info!(
            pid = %summary.pid,
            uid = summary.uid.as_raw(),
            servers_cleaned = summary.servers_cleaned,
            background_tasks_cleaned = summary.background_tasks_cleaned,
            %reason,
            "Worker cleanup complete"
        );
        Some(WorkerCleanupArtifacts {
            summary,
            background_tasks,
        })
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
                worker_tasks: HashMap::new(),
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
        self: &Arc<Self>,
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

        // Phase 2: name is vacant — resolve bind URIs and bind the server.
        let device_names = gm_quic::qinterface::device::Devices::global()
            .interfaces()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let bind_uris = crate::bind::resolve_bind_uris(&request.bind, &device_names);

        self.listeners
            .add_server(
                &server_name,
                request.identity.certs(),
                request.identity.key(),
                bind_uris,
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
                listens: request.bind,
            },
        );

        tracing::info!(%server_name, ?owner, "server registered");
        Ok(PerServerListener::new_registered(
            rx,
            shutdown_token,
            self,
            server_name,
            owner,
        ))
    }

    /// Release a single active server entry owned by the specified owner.
    pub async fn release_server(&self, server_name: &str, owner: ServiceOwner) {
        let gate = self.name_gate(server_name).await;
        let _guard = gate.lock().await;

        let mut inner = self.inner.lock().await;
        let owned = matches!(
            inner.servers.get(server_name),
            Some(ServerEntry::Active { owner: existing_owner, .. }) if *existing_owner == owner
        );
        if !owned {
            return;
        }

        inner.retire_server(server_name, &self.listeners);
        if let ServiceOwner::Worker(pid) = owner
            && let Some(process) = inner.processes.get_mut(&pid)
        {
            process.owned_servers.remove(server_name);
        }
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

    /// Reconcile bind URIs for active servers affected by a network
    /// interface event.
    ///
    /// Only servers whose [`Listens`] reference the changed device (via
    /// [`IfaceRange::All`] or [`IfaceRange::Exact`]) are re-resolved.
    /// Listens with `specific_addrs` are always skipped (they don't depend
    /// on network interfaces).
    pub async fn reconcile_binds(&self, event: &gm_quic::qinterface::device::InterfaceEvent) {
        let device = event.device();

        let device_names: Vec<String> = gm_quic::qinterface::device::Devices::global()
            .interfaces()
            .keys()
            .cloned()
            .collect();

        // Collect only servers whose listens are affected by this device.
        let entries: Vec<(String, Vec<gateway::parse::Listens>)> = {
            let inner = self.inner.lock().await;
            inner
                .servers
                .iter()
                .filter_map(|(name, entry)| match entry {
                    ServerEntry::Active { listens, .. } => {
                        let affected = listens
                            .iter()
                            .any(|l| l.specific_addrs.is_none() && l.range.contains(device));
                        affected.then(|| (name.clone(), listens.clone()))
                    }
                    ServerEntry::Conflicted => None,
                })
                .collect()
        };

        for (server_name, listens) in entries {
            let desired_uris: Vec<String> = crate::bind::resolve_bind_uris(&listens, &device_names);

            // Build identity_key → original URI maps for stable comparison.
            // alloc_port() generates a unique query param each call, so we
            // must compare by identity_key (URI without query string).
            let desired_keys: std::collections::HashMap<String, &str> = desired_uris
                .iter()
                .map(|uri| {
                    let bind_uri = gm_quic::prelude::BindUri::from(uri.as_str());
                    (bind_uri.identity_key(), uri.as_str())
                })
                .collect();

            let Some(server) = self.listeners.get_server(&server_name) else {
                continue;
            };

            let current_map: std::collections::HashMap<String, String> = server
                .bind_interfaces()
                .keys()
                .map(|uri| (uri.identity_key(), uri.to_string()))
                .collect();

            let desired_key_set: std::collections::HashSet<&String> = desired_keys.keys().collect();
            let current_key_set: std::collections::HashSet<&String> = current_map.keys().collect();

            // Bind new URIs (present in desired but not in current).
            let to_add: Vec<String> = desired_key_set
                .difference(&current_key_set)
                .filter_map(|key| desired_keys.get(key.as_str()).map(|s| s.to_string()))
                .collect();
            if !to_add.is_empty() {
                tracing::info!(
                    %server_name,
                    added = ?to_add,
                    "reconcile: binding new interfaces"
                );
                server.bind(to_add).await;
            }

            // Unbind removed URIs (present in current but not in desired).
            let to_remove: Vec<String> = current_key_set
                .difference(&desired_key_set)
                .filter_map(|key| current_map.get(key.as_str()).cloned())
                .collect();
            for uri_str in &to_remove {
                let uri = gm_quic::prelude::BindUri::from(uri_str.as_str());
                if let Some(iface) = server.remove_iface(&uri) {
                    let _ = iface.close().await;
                }
            }
            if !to_remove.is_empty() {
                tracing::info!(
                    %server_name,
                    removed = ?to_remove,
                    "reconcile: unbound removed interfaces"
                );
            }
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
                owned_servers: HashSet::new(),
                worker_handle,
            },
        );
        inner.users.insert(uid, pid);
        tracing::info!(pid = %pid, uid = uid.as_raw(), "Registered worker");
    }

    /// Check whether a worker with the given PID is registered.
    pub async fn has_worker(&self, pid: Pid) -> bool {
        self.inner.lock().await.processes.contains_key(&pid)
    }

    /// Spawn and track a root-side background task for a worker.
    pub async fn spawn_worker_task<F>(&self, pid: Pid, task: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let mut inner = self.inner.lock().await;
        inner
            .worker_tasks
            .entry(pid)
            .or_insert_with(JoinSet::new)
            .spawn(task);
    }

    /// Abort and drain any root-side background tasks associated with the pid.
    pub async fn cleanup_worker_tasks(&self, pid: Pid) {
        let mut tasks = {
            let mut inner = self.inner.lock().await;
            inner.worker_tasks.remove(&pid).unwrap_or_default()
        };
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }

    /// Remove all resources for a dead/exited worker process.
    pub async fn cleanup_worker_with_reason(
        &self,
        pid: Pid,
        reason: &str,
    ) -> Option<CleanupSummary> {
        let artifacts = self
            .inner
            .lock()
            .await
            .cleanup_worker(pid, reason, &self.listeners)?;

        let mut tasks = artifacts.background_tasks;
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}

        Some(artifacts.summary)
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
