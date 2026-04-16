//! Root-side ownership registry for server_name → local/worker mappings.
//!
//! Tracks which worker process owns which server names, provides conflict
//! detection, and manages the lifecycle of per-server listen adapters routed
//! from the shared [`Network`](h3x::endpoint::Network)'s SNI dispatcher.
//!
//! All mutating methods take `&self` and use interior mutability so that
//! `RootState` can be shared via `Arc` without external synchronization.

mod process_ops;
mod server_ops;

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use h3x::{
    dquic::{prelude::BindUri, qinterface::BindInterface},
    endpoint::{config::ServerQuicConfig, network::Network},
};
use nix::unistd::{Pid, Uid};
use snafu::Snafu;
use tokio::{
    sync::{Mutex, RwLock, mpsc},
    task::JoinSet,
};
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};

use crate::hypervisor::worker_handle::WorkerHandle;

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
    /// Failed to register the server's identity on the shared [`Network`].
    #[snafu(display("failed to bind server `{server_name}` on network"))]
    BindServer {
        server_name: String,
        source: h3x::endpoint::network::BindServerError,
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
    /// A registration is in progress — the async bind is running. Acts as
    /// a sentinel so concurrent callers see the name as occupied.
    /// Transitions to `Active` on success, removed on failure.
    Registering { owner: ServiceOwner },
    /// Name is actively owned and serving.
    Active {
        owner: ServiceOwner,
        /// Channel to route accepted connections into the worker/local
        /// service's [`PerServerListener`]. Retained only for legacy API
        /// compatibility; the real fanout is driven by `_accept_task`.
        conn_tx: mpsc::Sender<Arc<h3x::dquic::prelude::Connection>>,
        shutdown_token: CancellationToken,
        /// Original listen specifications for network-change reconciliation.
        listens: Vec<gateway::parse::Listens>,
        /// Bind URIs currently bound on the shared [`Network`] on behalf of
        /// this server. Each entry holds a live [`BindInterface`] to keep
        /// the underlying socket alive; dropping the entry releases one
        /// reference in [`InterfaceManager`], which closes the socket when
        /// it is the last outstanding reference.
        bound_ifaces: HashMap<BindUri, BindInterface>,
        /// SNI registration handle on the shared [`Network`]. Owns the
        /// fanout task that drains inbound connections into `conn_tx`;
        /// dropping releases the SNI entry.
        _accept_task: AbortOnDropHandle<()>,
        /// Per-server DNS publish task, aborted when the entry is retired.
        publish_task: Option<AbortOnDropHandle<()>>,
        /// Per-server stapled OCSP refresh task, aborted when the entry is retired.
        stapling_task: Option<AbortOnDropHandle<()>>,
    },
    /// Name is poisoned due to a cross-owner conflict.
    /// Only cleared by `scrub_conflicts()` during reload.
    Conflicted,
}

impl ServerEntry {
    /// Return the owner if this entry is `Registering` or `Active`.
    pub(super) fn owner(&self) -> Option<ServiceOwner> {
        match self {
            Self::Registering { owner } | Self::Active { owner, .. } => Some(*owner),
            Self::Conflicted => None,
        }
    }
}

/// Per-worker-process tracking record.
pub(super) struct WorkerProcessRecord {
    /// The UID this worker runs as.
    pub(super) uid: Uid,
    /// The username this worker runs as.
    pub(super) username: String,
    /// Set of server_names owned by this worker.
    pub(super) owned_servers: HashSet<String>,
    /// Handle to the spawned worker process.
    pub(super) worker_handle: WorkerHandle,
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

pub(super) struct Inner {
    /// pid → worker process info.
    pub(super) processes: HashMap<Pid, WorkerProcessRecord>,
    /// uid → pid mapping (one worker per uid).
    pub(super) users: HashMap<Uid, Pid>,
    /// Root-side background tasks grouped by worker pid.
    pub(super) worker_tasks: HashMap<Pid, JoinSet<()>>,
}

// ---------------------------------------------------------------------------
// RootState
// ---------------------------------------------------------------------------

/// Root-side ownership registry (thread-safe, interior mutability).
///
/// Tracks `server_name → owner`, `pid → owned_servers`, and `uid → pid`
/// mappings. Owns the shared [`Network`] + default [`ServerQuicConfig`]
/// used for every server registration, and coordinates all server
/// registration / cleanup.
pub struct RootState {
    /// Shared QUIC network with installed SNI dispatcher.
    pub network: Arc<Network>,
    /// Server-side QUIC/TLS configuration shared across every registered
    /// SNI. `Network::bind_server` rejects registrations whose config
    /// does not match this one, so a single instance is kept here and
    /// cloned (cheap — inner `Arc`s) for every `bind_server` call.
    pub server_qcfg: ServerQuicConfig,
    /// Server entries (behind RwLock for concurrent reads).
    pub(super) servers: RwLock<ServerRegistry>,
    /// Process/user bookkeeping (behind Mutex).
    pub(super) inner: Mutex<Inner>,
    /// Notified when SIGCHLD arrives so the monitor loop wakes immediately.
    pub worker_notify: tokio::sync::Notify,
}

/// Server-name registry: `server_name → ServerEntry` state machine.
///
/// Entry lifecycle: `(vacant) → Registering → Active → (removed)`.
/// Conflict: any state → `Conflicted`, cleared by `scrub_conflicts`.
pub(super) struct ServerRegistry {
    pub(super) entries: HashMap<String, ServerEntry>,
}

impl ServerRegistry {
    pub(super) fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Remove a server entry and return its accept task, if any.
    ///
    /// Callers **must** await the returned [`AbortOnDropHandle`] after
    /// releasing any locks — dropping it triggers the abort but the
    /// runtime drops the task's captured [`ServerBinding`] asynchronously,
    /// so a subsequent `bind_server` call for the same SNI may otherwise
    /// race against the old binding's `SniGuard::Drop` and fail with
    /// `SniInUse`. Awaiting the handle blocks until the task's locals
    /// (including the `ServerBinding`) have been dropped.
    ///
    /// For `Registering` sentinels there is no `ServerBinding` yet, so
    /// removal of the map entry is sufficient and `None` is returned.
    ///
    /// Caller must already hold a write lock on this `ServerRegistry`.
    pub(super) fn retire_entry(
        &mut self,
        server_name: &str,
    ) -> Option<tokio_util::task::AbortOnDropHandle<()>> {
        let entry = self.entries.remove(server_name)?;
        match entry {
            ServerEntry::Active {
                shutdown_token,
                _accept_task,
                ..
            } => {
                shutdown_token.cancel();
                // Abort the accept task eagerly so its captured
                // `ServerBinding` is released; callers await the handle to
                // observe the drop synchronously before the next
                // `bind_server` for the same SNI.
                _accept_task.abort();
                Some(_accept_task)
            }
            ServerEntry::Registering { .. } | ServerEntry::Conflicted => None,
        }
    }
}

impl RootState {
    /// Create a new root state with the given shared [`Network`] and
    /// default [`ServerQuicConfig`].
    pub fn new(network: Arc<Network>, server_qcfg: ServerQuicConfig) -> Self {
        Self {
            network,
            server_qcfg,
            servers: RwLock::new(ServerRegistry::new()),
            inner: Mutex::new(Inner {
                processes: HashMap::new(),
                users: HashMap::new(),
                worker_tasks: HashMap::new(),
            }),
            worker_notify: tokio::sync::Notify::new(),
        }
    }
}

#[cfg(test)]
mod tests;
