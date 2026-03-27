//! Root process state: server registry and worker registry.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use nix::{
    sys::signal::Signal,
    unistd::{Pid, Uid},
};
use snafu::{Report, Snafu};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::worker_spawn::WorkerHandle;

/// Identifies who owns a registered server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceOwner {
    /// Owned by the root process (root-local service).
    Local,
    /// Owned by a worker process identified by PID.
    Worker(Pid),
}

/// A registered server entry in the root state.
pub struct ServerEntry {
    pub owner: ServiceOwner,
    pub conn_tx: mpsc::Sender<gm_quic::prelude::Connection>,
    pub shutdown_token: CancellationToken,
}

/// A registered worker process entry.
pub struct WorkerEntry {
    pub uid: Uid,
    pub owned_servers: HashSet<String>,
    pub worker_handle: WorkerHandle,
    /// Cancellation tokens for connector serve futures owned by this worker.
    pub connector_shutdown_tokens: Vec<CancellationToken>,
}

/// Summary of a worker cleanup operation.
#[derive(Debug, Clone)]
pub struct CleanupSummary {
    pub pid: Pid,
    pub uid: Uid,
    pub servers_cleaned: usize,
    pub connectors_cleaned: usize,
}

/// Mutable registry state, protected by a [`Mutex`].
struct RegistryInner {
    /// server_name → server entry (connection routing)
    server_registry: HashMap<String, ServerEntry>,
    /// worker pid → worker entry
    worker_registry: HashMap<Pid, WorkerEntry>,
    /// uid → pid (enforces 1:1 constraint)
    uid_index: HashMap<Uid, Pid>,
}

/// Central state of the root process.
///
/// Tracks all registered servers (both root-local and worker-owned) and
/// all active worker processes. All methods take `&self` — interior
/// mutability is provided by a [`Mutex`] around the registry data.
/// The [`QuicListeners`](gm_quic::prelude::QuicListeners) field is
/// accessible without locking.
pub struct RootState {
    /// The shared QUIC listeners object.
    pub listeners: Arc<gm_quic::prelude::QuicListeners>,
    inner: Mutex<RegistryInner>,
}

impl RootState {
    pub fn new(listeners: Arc<gm_quic::prelude::QuicListeners>) -> Self {
        Self {
            listeners,
            inner: Mutex::new(RegistryInner {
                server_registry: HashMap::new(),
                worker_registry: HashMap::new(),
                uid_index: HashMap::new(),
            }),
        }
    }

    // -----------------------------------------------------------------------
    // Server registry
    // -----------------------------------------------------------------------

    /// Check whether a server name is already registered.
    pub fn has_server(&self, server_name: &str) -> bool {
        self.inner
            .lock()
            .unwrap()
            .server_registry
            .contains_key(server_name)
    }

    /// Register a new server. Returns `Err` if the name is already taken.
    pub fn register_server(
        &self,
        name: String,
        entry: ServerEntry,
    ) -> Result<(), RegisterServerError> {
        let mut inner = self.inner.lock().unwrap();
        snafu::ensure!(
            !inner.server_registry.contains_key(&name),
            register_server_error::ConflictSnafu { name: &name }
        );
        if let ServiceOwner::Worker(pid) = &entry.owner
            && let Some(worker) = inner.worker_registry.get_mut(pid)
        {
            worker.owned_servers.insert(name.clone());
        }
        inner.server_registry.insert(name, entry);
        Ok(())
    }

    /// Unregister a server. Checks that the caller owns it.
    pub fn unregister_server(
        &self,
        name: &str,
        caller: &ServiceOwner,
    ) -> Result<ServerEntry, UnregisterServerError> {
        let mut inner = self.inner.lock().unwrap();
        let entry =
            inner
                .server_registry
                .get(name)
                .ok_or_else(|| UnregisterServerError::NotFound {
                    name: name.to_owned(),
                })?;
        snafu::ensure!(
            &entry.owner == caller,
            unregister_server_error::NotOwnerSnafu { name }
        );
        let entry = inner.server_registry.remove(name).unwrap();
        entry.shutdown_token.cancel();
        self.listeners.remove_server(name);
        if let ServiceOwner::Worker(pid) = &entry.owner
            && let Some(worker) = inner.worker_registry.get_mut(pid)
        {
            worker.owned_servers.remove(name);
        }
        Ok(entry)
    }

    /// Look up the connection sender for a server name (used by the routing loop).
    pub fn get_conn_sender(
        &self,
        server_name: &str,
    ) -> Option<mpsc::Sender<gm_quic::prelude::Connection>> {
        self.inner
            .lock()
            .unwrap()
            .server_registry
            .get(server_name)
            .map(|e| e.conn_tx.clone())
    }

    /// Get the owner of a server name.
    pub fn server_owner(&self, server_name: &str) -> Option<ServiceOwner> {
        self.inner
            .lock()
            .unwrap()
            .server_registry
            .get(server_name)
            .map(|e| e.owner.clone())
    }

    /// Retire a single server: cancel its adapter, remove from listeners.
    fn retire_server(
        inner: &mut RegistryInner,
        listeners: &gm_quic::prelude::QuicListeners,
        server_name: &str,
    ) -> Option<()> {
        let entry = inner.server_registry.remove(server_name)?;
        entry.shutdown_token.cancel();
        listeners.remove_server(server_name);
        Some(())
    }

    /// Retire all root-local servers. Returns the list of retired names.
    pub fn retire_local_servers(&self) -> Vec<String> {
        let mut inner = self.inner.lock().unwrap();
        let local_names: Vec<String> = inner
            .server_registry
            .iter()
            .filter_map(|(name, entry)| {
                (entry.owner == ServiceOwner::Local).then_some(name.clone())
            })
            .collect();

        for name in &local_names {
            let _ = Self::retire_server(&mut inner, &self.listeners, name);
        }
        local_names
    }

    // -----------------------------------------------------------------------
    // Worker registry
    // -----------------------------------------------------------------------

    /// Check whether a worker with the given PID is registered.
    pub fn has_worker(&self, pid: Pid) -> bool {
        self.inner
            .lock()
            .unwrap()
            .worker_registry
            .contains_key(&pid)
    }

    /// Register a new worker process.
    pub fn register_worker(&self, pid: Pid, uid: Uid, worker_handle: WorkerHandle) {
        let mut inner = self.inner.lock().unwrap();
        // If an existing worker has the same UID, clean it up first.
        if let Some(old_pid) = inner.uid_index.get(&uid).copied()
            && old_pid != pid
        {
            Self::cleanup_worker_inner(&mut inner, &self.listeners, old_pid, "uid_replaced");
        }
        inner.worker_registry.insert(
            pid,
            WorkerEntry {
                uid,
                owned_servers: HashSet::new(),
                worker_handle,
                connector_shutdown_tokens: Vec::new(),
            },
        );
        inner.uid_index.insert(uid, pid);
        tracing::info!(pid = %pid, uid = uid.as_raw(), "Registered worker");
    }

    /// Add a connector cancellation token to a worker's record.
    pub fn add_connector_token(&self, pid: Pid, token: CancellationToken) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(worker) = inner.worker_registry.get_mut(&pid) {
            worker.connector_shutdown_tokens.push(token);
        }
    }

    /// Clean up a worker: remove all its servers, cancel connectors, remove from registries.
    pub fn cleanup_worker(&self, pid: Pid) -> Option<CleanupSummary> {
        let mut inner = self.inner.lock().unwrap();
        Self::cleanup_worker_inner(&mut inner, &self.listeners, pid, "cleanup")
    }

    pub fn cleanup_worker_with_reason(&self, pid: Pid, reason: &str) -> Option<CleanupSummary> {
        let mut inner = self.inner.lock().unwrap();
        Self::cleanup_worker_inner(&mut inner, &self.listeners, pid, reason)
    }

    fn cleanup_worker_inner(
        inner: &mut RegistryInner,
        listeners: &gm_quic::prelude::QuicListeners,
        pid: Pid,
        reason: &str,
    ) -> Option<CleanupSummary> {
        let worker = inner.worker_registry.remove(&pid)?;

        if inner.uid_index.get(&worker.uid).copied() == Some(pid) {
            inner.uid_index.remove(&worker.uid);
        }

        let mut servers_cleaned = 0usize;
        for server_name in &worker.owned_servers {
            if Self::retire_server(inner, listeners, server_name).is_some() {
                servers_cleaned += 1;
            }
        }

        let connectors_cleaned = worker.connector_shutdown_tokens.len();
        for token in &worker.connector_shutdown_tokens {
            token.cancel();
        }

        let summary = CleanupSummary {
            pid,
            uid: worker.uid,
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

    /// Collect PIDs of workers that have exited.
    pub fn collect_exited_workers(&self) -> Vec<Pid> {
        let mut inner = self.inner.lock().unwrap();
        let mut exited = Vec::new();
        for (pid, worker) in &mut inner.worker_registry {
            match worker.worker_handle.try_wait() {
                Ok(Some(status)) => {
                    tracing::warn!(pid = %pid, ?status, "Worker exited");
                    exited.push(*pid);
                }
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

    /// Send SIGKILL to all workers. Returns killed PIDs.
    pub fn force_kill_workers(&self, reason: &str) -> Vec<Pid> {
        let mut inner = self.inner.lock().unwrap();
        let mut killed = Vec::new();
        for (pid, worker) in &mut inner.worker_registry {
            match worker.worker_handle.start_kill() {
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

    /// Forward a Unix signal to all workers.
    pub fn forward_unix_signal(&self, signal: Signal) {
        let inner = self.inner.lock().unwrap();
        for (pid, worker) in &inner.worker_registry {
            let Some(raw_pid) = worker.worker_handle.pid() else {
                continue;
            };
            let child_pid = Pid::from_raw(raw_pid as i32);
            if let Err(error) = nix::sys::signal::kill(child_pid, signal) {
                tracing::warn!(
                    pid = %pid,
                    error = %Report::from_error(&error),
                    ?signal,
                    "Failed to forward signal to worker"
                );
            }
        }
    }

    /// Get all active worker PIDs.
    pub fn worker_pids(&self) -> Vec<Pid> {
        self.inner
            .lock()
            .unwrap()
            .worker_registry
            .keys()
            .copied()
            .collect()
    }

    /// Get the PID for a given UID.
    pub fn get_pid_for_uid(&self, uid: Uid) -> Option<Pid> {
        self.inner.lock().unwrap().uid_index.get(&uid).copied()
    }
}

// ---------------------------------------------------------------------------
// Per-operation error types
// ---------------------------------------------------------------------------

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum RegisterServerError {
    #[snafu(display("server `{name}` conflicts with an existing listener"))]
    Conflict { name: String },
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum UnregisterServerError {
    #[snafu(display("server `{name}` not found"))]
    NotFound { name: String },
    #[snafu(display("caller does not own server `{name}`"))]
    NotOwner { name: String },
}
