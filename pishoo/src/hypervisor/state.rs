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
use snafu::{Report, Snafu};
use tokio::{
    sync::{Mutex, RwLock, mpsc},
    task::JoinSet,
};
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};
use tracing::Instrument;

use crate::{hypervisor::worker_handle::WorkerHandle, listen::PerServerListener};

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
        source: dquic::prelude::ServerError,
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
    /// A registration is in progress — the async `add_server` call is
    /// running. Acts as a sentinel so that concurrent callers see the name
    /// as occupied. Transitions to `Active` on success, removed on failure.
    Registering { owner: ServiceOwner },
    /// Name is actively owned and serving.
    Active {
        owner: ServiceOwner,
        conn_tx: mpsc::Sender<dquic::prelude::Connection>,
        shutdown_token: CancellationToken,
        /// Original listen specifications for network-change reconciliation.
        listens: Vec<gateway::parse::Listens>,
        /// Per-server DNS publish task, aborted when the entry is retired.
        publish_task: Option<AbortOnDropHandle<()>>,
    },
    /// Name is poisoned due to a cross-owner conflict.
    /// Only cleared by `scrub_conflicts()` during reload.
    Conflicted,
}

impl ServerEntry {
    /// Return the owner if this entry is `Registering` or `Active`.
    fn owner(&self) -> Option<ServiceOwner> {
        match self {
            Self::Registering { owner } | Self::Active { owner, .. } => Some(*owner),
            Self::Conflicted => None,
        }
    }
}

/// Per-worker-process tracking record.
struct WorkerProcessRecord {
    /// The UID this worker runs as.
    uid: Uid,
    /// The username this worker runs as.
    username: String,
    /// Set of server_names owned by this worker.
    owned_servers: HashSet<String>,
    /// Handle to the spawned worker process.
    worker_handle: WorkerHandle,
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
    /// pid → worker process info.
    processes: HashMap<Pid, WorkerProcessRecord>,
    /// uid → pid mapping (one worker per uid).
    users: HashMap<Uid, Pid>,
    /// Root-side background tasks grouped by worker pid.
    worker_tasks: HashMap<Pid, JoinSet<()>>,
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
    pub listeners: Arc<dquic::prelude::QuicListeners>,
    /// Server entries (behind RwLock for concurrent reads).
    servers: RwLock<ServerRegistry>,
    /// Process/user bookkeeping (behind Mutex).
    inner: Mutex<Inner>,
    /// Notified when SIGCHLD arrives so the monitor loop wakes immediately.
    pub worker_notify: tokio::sync::Notify,
}

/// Server-name registry: `server_name → ServerEntry` state machine.
///
/// Entry lifecycle: `(vacant) → Registering → Active → (removed)`.
/// Conflict: any state → `Conflicted`, cleared by `scrub_conflicts`.
struct ServerRegistry {
    entries: HashMap<String, ServerEntry>,
}

impl ServerRegistry {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Remove a server entry, cancel its listener, and remove from QuicListeners.
    ///
    /// For `Registering` entries, calls `remove_server` defensively (the async
    /// `add_server` may have already completed). `remove_server` is idempotent.
    ///
    /// Caller must already hold a write lock on this `ServerRegistry`.
    fn retire_entry(
        &mut self,
        server_name: &str,
        listeners: &dquic::prelude::QuicListeners,
    ) -> Option<()> {
        let entry = self.entries.remove(server_name)?;
        match entry {
            ServerEntry::Active { shutdown_token, .. } => {
                shutdown_token.cancel();
                listeners.remove_server(server_name);
            }
            ServerEntry::Registering { .. } => {
                // add_server may or may not have completed — idempotent cleanup.
                listeners.remove_server(server_name);
            }
            ServerEntry::Conflicted => {}
        }
        Some(())
    }
}

impl RootState {
    /// Create a new root state with the given shared QUIC listeners.
    pub fn new(listeners: Arc<dquic::prelude::QuicListeners>) -> Self {
        Self {
            listeners,
            servers: RwLock::new(ServerRegistry::new()),
            inner: Mutex::new(Inner {
                processes: HashMap::new(),
                users: HashMap::new(),
                worker_tasks: HashMap::new(),
            }),
            worker_notify: tokio::sync::Notify::new(),
        }
    }

    // -----------------------------------------------------------------------
    // Server registry
    // -----------------------------------------------------------------------

    /// Unified entry point for registering a listener.
    ///
    /// State machine:
    /// - **Vacant** → insert `Registering` sentinel → async `add_server` →
    ///   promote to `Active`.
    /// - **Registering/Active, same owner** → `DuplicateListen`.
    /// - **Registering/Active, different owner** → poison to `Conflicted`.
    /// - **Conflicted** → `ConflictedName`.
    ///
    /// Returns a [`PerServerListener`] on success.
    pub async fn register_listener(
        self: &Arc<Self>,
        owner: ServiceOwner,
        request: ListenRequest,
    ) -> Result<PerServerListener, RegisterError> {
        let server_name = request.identity.name().as_full().to_owned();

        // Phase 1: claim the name by inserting a `Registering` sentinel.
        {
            let mut registry = self.servers.write().await;
            match registry.entries.get(&server_name) {
                Some(entry) if entry.owner() == Some(owner) => {
                    return Err(RegisterError::DuplicateListen);
                }
                Some(ServerEntry::Conflicted) => {
                    return Err(RegisterError::ConflictedName);
                }
                Some(_) => {
                    // Different owner occupies the name — conflict + poison.
                    let old = registry
                        .entries
                        .insert(server_name.clone(), ServerEntry::Conflicted);
                    // Release write lock before acquiring inner lock.
                    drop(registry);

                    if let Some(ServerEntry::Active {
                        shutdown_token,
                        owner: old_owner,
                        ..
                    }) = old
                    {
                        shutdown_token.cancel();
                        self.listeners.remove_server(&server_name);
                        if let ServiceOwner::Worker(pid) = old_owner {
                            let mut inner = self.inner.lock().await;
                            if let Some(proc) = inner.processes.get_mut(&pid) {
                                proc.owned_servers.remove(&server_name);
                            }
                        }
                    }
                    tracing::warn!(
                        %server_name,
                        new_owner = ?owner,
                        "cross-owner conflict: name poisoned"
                    );
                    return Err(RegisterError::ConflictedName);
                }
                None => {
                    // Vacant — claim with sentinel.
                    registry
                        .entries
                        .insert(server_name.clone(), ServerEntry::Registering { owner });
                }
            }
        }

        // Phase 2: name is claimed — resolve bind URIs and bind the server.
        // No lock held — other server_names can be read/written concurrently.
        let device_names = dquic::qinterface::device::Devices::global()
            .interfaces()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let bind_uris = crate::bind::resolve_bind_uris(&request.bind, &device_names);

        let add_result = self
            .listeners
            .add_server(
                &server_name,
                request.identity.certs(),
                request.identity.key(),
                bind_uris,
                None::<Vec<u8>>,
            )
            .await;

        if let Err(source) = add_result {
            // Rollback the sentinel.
            self.servers.write().await.entries.remove(&server_name);
            return Err(RegisterError::AddServerFailed {
                server_name,
                source,
            });
        }

        // Phase 3: promote sentinel to `Active`.
        {
            let mut registry = self.servers.write().await;

            // Verify our sentinel is still there. Another operation (conflict
            // or cleanup) may have replaced/removed it.
            match registry.entries.get(&server_name) {
                Some(ServerEntry::Registering { owner: o }) if *o == owner => {
                    // Good — our sentinel is intact.
                }
                _ => {
                    // Sentinel was replaced (e.g., by cross-owner conflict or
                    // cleanup). Roll back the listeners binding.
                    self.listeners.remove_server(&server_name);
                    tracing::warn!(
                        %server_name,
                        ?owner,
                        "sentinel lost during register_listener; rolled back"
                    );
                    return Err(RegisterError::ConflictedName);
                }
            }

            let (tx, rx) = mpsc::channel(128);
            let shutdown_token = CancellationToken::new();

            // Spawn per-server DNS publish task.
            let publish_config = gateway::dns::build_publish_config_from_identity(
                &request.identity,
                request.dns_resolver_url.as_deref(),
            );
            let publish_task = if publish_config.resolvers.is_empty() {
                None
            } else {
                Some(gateway::dns::spawn_server_publish_task(
                    server_name.clone(),
                    publish_config,
                    self.listeners.clone(),
                ))
            };

            registry.entries.insert(
                server_name.clone(),
                ServerEntry::Active {
                    owner,
                    conn_tx: tx,
                    shutdown_token: shutdown_token.clone(),
                    listens: request.bind,
                    publish_task,
                },
            );
            drop(registry);

            // Track in the worker's owned_servers set.
            if let ServiceOwner::Worker(pid) = owner {
                let mut inner = self.inner.lock().await;
                if let Some(process) = inner.processes.get_mut(&pid) {
                    process.owned_servers.insert(server_name.clone());
                } else {
                    // Worker died during async gap — rollback.
                    drop(inner);
                    self.servers
                        .write()
                        .await
                        .retire_entry(&server_name, &self.listeners);
                    tracing::warn!(
                        %server_name,
                        pid = %pid,
                        "worker vanished during register_listener; rolled back"
                    );
                    return Err(RegisterError::ConflictedName);
                }
            }

            tracing::debug!(%server_name, ?owner, "server registered");
            Ok(PerServerListener::new_registered(
                rx,
                shutdown_token,
                self,
                server_name,
                owner,
            ))
        }
    }

    /// Release a single active server entry owned by the specified owner.
    pub async fn release_server(&self, server_name: &str, owner: ServiceOwner) {
        {
            let mut registry = self.servers.write().await;
            let owned = matches!(
                registry.entries.get(server_name),
                Some(ServerEntry::Active { owner: existing_owner, .. }) if *existing_owner == owner
            );
            if !owned {
                return;
            }
            registry.retire_entry(server_name, &self.listeners);
        }

        if let ServiceOwner::Worker(pid) = owner {
            let mut inner = self.inner.lock().await;
            if let Some(process) = inner.processes.get_mut(&pid) {
                process.owned_servers.remove(server_name);
            }
        }
    }

    /// Look up the routing sender for a given server_name.
    ///
    /// Used by the central accept loop to route connections to the correct
    /// per-server adapter. Returns `None` for `Conflicted` entries.
    pub async fn get_conn_sender(
        &self,
        server_name: &str,
    ) -> Option<mpsc::Sender<dquic::prelude::Connection>> {
        let registry = self.servers.read().await;
        match registry.entries.get(server_name) {
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
    pub async fn reconcile_binds(&self, event: &dquic::qinterface::device::InterfaceEvent) {
        let device = event.device();

        let device_names: Vec<String> = dquic::qinterface::device::Devices::global()
            .interfaces()
            .keys()
            .cloned()
            .collect();

        // Collect only servers whose listens are affected by this device.
        let entries: Vec<(String, Vec<gateway::parse::Listens>)> = {
            let registry = self.servers.read().await;
            registry
                .entries
                .iter()
                .filter_map(|(name, entry)| match entry {
                    ServerEntry::Active { listens, .. } => {
                        let affected = listens
                            .iter()
                            .any(|l| l.specific_addrs.is_none() && l.range.contains(device));
                        affected.then(|| (name.clone(), listens.clone()))
                    }
                    ServerEntry::Registering { .. } | ServerEntry::Conflicted => None,
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
                    let bind_uri = dquic::prelude::BindUri::from(uri.as_str());
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
                let uri = dquic::prelude::BindUri::from(uri_str.as_str());
                if let Some(iface) = server.remove_iface(&uri) {
                    // Close the interface in the background to avoid blocking
                    // the reconcile loop. BindInterface::close() waits for all
                    // components (e.g. STUN keep-alive tasks) to shut down,
                    // which may take a long time if the network interface has
                    // already been removed at the OS level.
                    tokio::spawn(
                        async move {
                            let _ = iface.close().await;
                        }
                        .in_current_span(),
                    );
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

    /// Remove all `Conflicted` entries from the registry.
    ///
    /// Called during reload (SIGHUP) **before** forwarding the signal to
    /// workers, so that workers can re-register previously-conflicted names.
    pub async fn scrub_conflicts(&self) -> Vec<String> {
        let mut registry = self.servers.write().await;
        let conflicted: Vec<String> = registry
            .entries
            .iter()
            .filter_map(|(name, entry)| {
                matches!(entry, ServerEntry::Conflicted).then_some(name.clone())
            })
            .collect();

        for name in &conflicted {
            registry.entries.remove(name);
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
        let servers_cleaned = {
            let mut registry = self.servers.write().await;
            let mut cleaned = 0usize;
            for server_name in &owned_servers {
                let dominated = matches!(
                    registry.entries.get(server_name.as_str()),
                    Some(ServerEntry::Active { owner: ServiceOwner::Worker(p), .. }) if *p == pid
                );
                if dominated {
                    registry.retire_entry(server_name, &self.listeners);
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
                registry.retire_entry(name, &self.listeners);
                cleaned += 1;
            }
            cleaned
        };
        // servers lock released here.

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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use dhttp_home::identity::{Name, ssl::Identity};
    use gateway::control_plane::ListenRequest;
    use nix::unistd::{Pid, Uid};

    use super::*;
    use crate::hypervisor::worker_handle::WorkerHandle;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Create an independent QuicListeners + RootState for one test.
    fn test_state() -> Arc<RootState> {
        // Ensure the rustls CryptoProvider is installed (idempotent).
        let _ = rustls::crypto::ring::default_provider().install_default();

        let router = Arc::new(dquic::qinterface::component::route::QuicRouter::new());
        let listeners = dquic::prelude::QuicListeners::builder()
            .with_router(router)
            .without_client_cert_verifier()
            .with_alpns([b"h3".as_slice()])
            .listen(8)
            .expect("failed to create test QuicListeners");
        Arc::new(RootState::new(listeners))
    }

    /// Generate a self-signed Identity for `{label}.user.genmeta.net`.
    fn test_identity(label: &str) -> Identity {
        let fqdn = format!("{label}.user.genmeta.net");
        let key_pair = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(vec![fqdn.clone()]).unwrap();
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, &fqdn);
        let cert = params.self_signed(&key_pair).unwrap();
        let cert_der = rustls::pki_types::CertificateDer::from(cert.der().to_vec());
        let key_der = rustls::pki_types::PrivateKeyDer::try_from(key_pair.serialize_der()).unwrap();
        let name = Name::try_from_str(&fqdn).unwrap().into_owned();
        Identity::new(name, vec![cert_der], key_der)
    }

    /// Build a ListenRequest with no bind addresses (no actual sockets).
    fn test_request(label: &str) -> ListenRequest {
        ListenRequest {
            identity: test_identity(label),
            bind: vec![],
            dns_resolver_url: None,
        }
    }

    /// Create a WorkerHandle from a raw PID value.
    ///
    /// Uses PID 1 (init) by default in tests — never killed because our
    /// tests never call force_kill / drop the handle with intent to reap.
    fn fake_worker_handle(raw: i32) -> WorkerHandle {
        WorkerHandle::from_pid(Pid::from_raw(raw))
    }

    // -----------------------------------------------------------------------
    // Phase A: Worker registry
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_register_worker_basic() {
        let state = test_state();
        let pid = Pid::from_raw(99999);
        let uid = Uid::from_raw(1000);

        state
            .register_worker(pid, uid, "alice".into(), fake_worker_handle(99999))
            .await;

        assert!(state.has_worker(pid).await);
        assert_eq!(state.pid_for_uid(uid).await, Some(pid));
        assert!(state.worker_pids().await.contains(&pid));
    }

    #[tokio::test]
    async fn test_register_worker_uid_replacement() {
        let state = test_state();
        let uid = Uid::from_raw(1001);
        let old_pid = Pid::from_raw(88881);
        let new_pid = Pid::from_raw(88882);

        state
            .register_worker(old_pid, uid, "bob".into(), fake_worker_handle(88881))
            .await;
        assert!(state.has_worker(old_pid).await);

        // Same UID, different PID → old worker is cleaned up.
        state
            .register_worker(new_pid, uid, "bob".into(), fake_worker_handle(88882))
            .await;

        assert!(!state.has_worker(old_pid).await);
        assert!(state.has_worker(new_pid).await);
        assert_eq!(state.pid_for_uid(uid).await, Some(new_pid));
    }

    #[tokio::test]
    async fn test_cleanup_unknown_pid() {
        let state = test_state();
        let result = state
            .cleanup_worker_with_reason(Pid::from_raw(77777), "test")
            .await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_cleanup_idempotent() {
        let state = test_state();
        let pid = Pid::from_raw(66666);
        state
            .register_worker(
                pid,
                Uid::from_raw(1002),
                "carol".into(),
                fake_worker_handle(66666),
            )
            .await;

        let first = state.cleanup_worker_with_reason(pid, "exit").await;
        assert!(first.is_some());

        let second = state.cleanup_worker_with_reason(pid, "exit").await;
        assert!(second.is_none());
    }

    #[tokio::test]
    async fn test_spawn_task_unregistered() {
        let state = test_state();
        let (tx, mut rx) = tokio::sync::oneshot::channel();

        // Task for an unregistered PID should not be spawned.
        state
            .spawn_worker_task(Pid::from_raw(55555), async move {
                let _ = tx.send(());
            })
            .await;

        // Give the runtime a chance to run the task (if it were spawned).
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err(), "task should not have been spawned");
    }

    #[tokio::test]
    async fn test_worker_pids_after_cleanup() {
        let state = test_state();
        let p1 = Pid::from_raw(44441);
        let p2 = Pid::from_raw(44442);

        state
            .register_worker(
                p1,
                Uid::from_raw(2001),
                "d".into(),
                fake_worker_handle(44441),
            )
            .await;
        state
            .register_worker(
                p2,
                Uid::from_raw(2002),
                "e".into(),
                fake_worker_handle(44442),
            )
            .await;

        assert_eq!(state.worker_pids().await.len(), 2);

        state.cleanup_worker_with_reason(p1, "exit").await;

        let pids = state.worker_pids().await;
        assert_eq!(pids.len(), 1);
        assert!(pids.contains(&p2));
    }

    // -----------------------------------------------------------------------
    // Phase B: Server lifecycle (via public API)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_register_then_release() {
        let state = test_state();
        let owner = ServiceOwner::Local;
        let server_name = "reg-release.user.genmeta.net";

        let _listener = state
            .register_listener(owner, test_request("reg-release"))
            .await
            .expect("register_listener should succeed");

        assert!(state.get_conn_sender(server_name).await.is_some());

        state.release_server(server_name, owner).await;
        assert!(state.get_conn_sender(server_name).await.is_none());
    }

    #[tokio::test]
    async fn test_register_duplicate_same_owner() {
        let state = test_state();
        let owner = ServiceOwner::Local;

        let _listener = state
            .register_listener(owner, test_request("dup-same"))
            .await
            .expect("first register should succeed");

        let err = state
            .register_listener(owner, test_request("dup-same"))
            .await
            .err()
            .expect("second register should fail");

        assert!(matches!(err, RegisterError::DuplicateListen));
    }

    #[tokio::test]
    async fn test_register_cross_owner_conflict() {
        let state = test_state();
        let owner_local = ServiceOwner::Local;
        let pid = Pid::from_raw(33333);
        let uid = Uid::from_raw(3000);
        let owner_worker = ServiceOwner::Worker(pid);

        state
            .register_worker(pid, uid, "worker".into(), fake_worker_handle(33333))
            .await;

        let _listener = state
            .register_listener(owner_local, test_request("cross-conflict"))
            .await
            .expect("first register should succeed");

        // Different owner for same name → ConflictedName.
        let err = state
            .register_listener(owner_worker, test_request("cross-conflict"))
            .await
            .err()
            .expect("cross-owner should conflict");

        assert!(matches!(err, RegisterError::ConflictedName));

        // Original server's conn_sender should be None (poisoned).
        assert!(
            state
                .get_conn_sender("cross-conflict.user.genmeta.net")
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn test_register_on_conflicted() {
        let state = test_state();
        let pid = Pid::from_raw(22221);
        state
            .register_worker(
                pid,
                Uid::from_raw(4000),
                "w1".into(),
                fake_worker_handle(22221),
            )
            .await;

        // Create a conflict first.
        let _l = state
            .register_listener(ServiceOwner::Local, test_request("on-conflict"))
            .await
            .unwrap();
        let _ = state
            .register_listener(ServiceOwner::Worker(pid), test_request("on-conflict"))
            .await;

        // Name is now Conflicted. Any new register should fail.
        let err = state
            .register_listener(ServiceOwner::Local, test_request("on-conflict"))
            .await
            .err()
            .expect("register on conflicted should fail");
        assert!(matches!(err, RegisterError::ConflictedName));
    }

    #[tokio::test]
    async fn test_scrub_then_reregister() {
        let state = test_state();
        let pid = Pid::from_raw(22222);
        state
            .register_worker(
                pid,
                Uid::from_raw(4001),
                "w2".into(),
                fake_worker_handle(22222),
            )
            .await;

        // Create conflict.
        let _l = state
            .register_listener(ServiceOwner::Local, test_request("scrub-re"))
            .await
            .unwrap();
        let _ = state
            .register_listener(ServiceOwner::Worker(pid), test_request("scrub-re"))
            .await;

        // Scrub should clear the conflict.
        let scrubbed = state.scrub_conflicts().await;
        assert!(scrubbed.contains(&"scrub-re.user.genmeta.net".to_owned()));

        // Re-register should now succeed.
        let _listener = state
            .register_listener(ServiceOwner::Local, test_request("scrub-re"))
            .await
            .expect("re-register after scrub should succeed");
        assert!(
            state
                .get_conn_sender("scrub-re.user.genmeta.net")
                .await
                .is_some()
        );
    }

    #[tokio::test]
    async fn test_release_wrong_owner() {
        let state = test_state();
        let pid = Pid::from_raw(11111);
        state
            .register_worker(
                pid,
                Uid::from_raw(5000),
                "w3".into(),
                fake_worker_handle(11111),
            )
            .await;

        let _listener = state
            .register_listener(ServiceOwner::Local, test_request("wrong-owner"))
            .await
            .unwrap();

        // Release with wrong owner should be a no-op.
        state
            .release_server("wrong-owner.user.genmeta.net", ServiceOwner::Worker(pid))
            .await;

        // Server should still be active.
        assert!(
            state
                .get_conn_sender("wrong-owner.user.genmeta.net")
                .await
                .is_some()
        );
    }

    #[tokio::test]
    async fn test_release_nonexistent() {
        let state = test_state();
        // Should not panic.
        state
            .release_server("does-not-exist.user.genmeta.net", ServiceOwner::Local)
            .await;
    }

    #[tokio::test]
    async fn test_cleanup_worker_releases_servers() {
        let state = test_state();
        let pid = Pid::from_raw(10001);
        let uid = Uid::from_raw(6000);
        let owner = ServiceOwner::Worker(pid);

        state
            .register_worker(pid, uid, "cleaner".into(), fake_worker_handle(10001))
            .await;

        let _listener = state
            .register_listener(owner, test_request("cleanup-srv"))
            .await
            .expect("register should succeed");
        assert!(
            state
                .get_conn_sender("cleanup-srv.user.genmeta.net")
                .await
                .is_some()
        );

        // Cleanup the worker — its server should also be gone.
        let summary = state
            .cleanup_worker_with_reason(pid, "exit")
            .await
            .expect("cleanup should find the worker");
        assert_eq!(summary.servers_cleaned, 1);

        assert!(
            state
                .get_conn_sender("cleanup-srv.user.genmeta.net")
                .await
                .is_none()
        );
        assert!(!state.has_worker(pid).await);
    }
}
