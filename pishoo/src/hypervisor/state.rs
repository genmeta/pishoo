//! Root-side ownership registry for server_name → local/worker mappings.
//!
//! Tracks which worker process owns which server names, provides conflict
//! detection, and manages the lifecycle of per-server DHTTP endpoints built
//! on the shared [`Network`](h3x::dquic::Network).
//!
//! All mutating methods take `&self` and use interior mutability so that
//! `RootState` can be shared via `Arc` without external synchronization.

mod process_ops;
mod server_ops;

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use dhttp::{endpoint::Endpoint, name::DhttpName};
use h3x::{
    dquic::{Network, server::ServerQuicConfig},
    quic::Listen as _,
};
use nix::unistd::{Pid, Uid};
use snafu::{Report, Snafu};
use tokio::{
    sync::{Mutex, RwLock},
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
    #[snafu(display("failed to build listen bind patterns"))]
    BuildBindPatterns {
        source: gateway::parse::types::ListenBindPatternError,
    },
    #[snafu(display("failed to build dns resolver for registered endpoint"))]
    BuildResolver { source: BuildEndpointResolverError },
    #[snafu(display("failed to build registered endpoint"))]
    BuildEndpoint {
        source: dhttp::endpoint::InvalidEndpointIdentityError,
    },
    #[snafu(display("failed to create dns publisher for registered endpoint"))]
    CreatePublisher {
        source: dhttp::ddns::CreatePublisherError,
    },
}

#[derive(Debug, Snafu)]
#[snafu(module(build_endpoint_resolver_error))]
pub enum BuildEndpointResolverError {
    #[snafu(display("failed to build dns endpoint"))]
    BuildEndpoint {
        source: dhttp::endpoint::InvalidEndpointIdentityError,
    },
    #[snafu(display("failed to attach h3 resolver"))]
    H3Resolver { source: std::io::Error },
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
        /// DHTTP endpoint built for this registered server.
        endpoint: Endpoint,
        /// Cancels accept calls currently blocked on this endpoint wrapper.
        shutdown_token: CancellationToken,
        /// Per-server DNS publish task, aborted when the entry is retired.
        publish_task: Option<AbortOnDropHandle<()>>,
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
    pub(super) owned_servers: HashSet<DhttpName<'static>>,
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
    /// SNI. A single instance is kept here and cloned (cheap — inner `Arc`s)
    /// for every DHTTP endpoint built by the root.
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
    pub(super) entries: HashMap<DhttpName<'static>, ServerEntry>,
}

pub(super) struct RetiredServer {
    endpoint: Endpoint,
    shutdown_token: CancellationToken,
    publish_task: Option<AbortOnDropHandle<()>>,
}

impl RetiredServer {
    pub(super) async fn shutdown(self) {
        self.shutdown_token.cancel();
        if let Some(task) = self.publish_task {
            task.abort();
            let _ = task.await;
        }
        if let Err(error) = self.endpoint.shutdown().await {
            tracing::warn!(
                error = %Report::from_error(&error),
                "failed to shut down retired endpoint"
            );
        }
    }
}

impl ServerRegistry {
    pub(super) fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Remove a server entry and return the endpoint to shut down, if any.
    ///
    /// Callers **must** shut down the returned endpoint after releasing any
    /// locks so the SNI binding and bind handles are released before a later
    /// registration for the same name.
    ///
    /// For `Registering` sentinels there is no endpoint yet, so removal of the
    /// map entry is sufficient and `None` is returned.
    ///
    /// Caller must already hold a write lock on this `ServerRegistry`.
    pub(super) fn retire_entry(
        &mut self,
        server_name: &DhttpName<'static>,
    ) -> Option<RetiredServer> {
        let entry = self.entries.remove(server_name)?;
        match entry {
            ServerEntry::Active {
                endpoint,
                shutdown_token,
                publish_task,
                ..
            } => Some(RetiredServer {
                endpoint,
                shutdown_token,
                publish_task,
            }),
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
