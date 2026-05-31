//! Root-side ownership registry for server_name → local/worker mappings.
//!
//! Tracks which worker process owns which server names, provides conflict
//! detection, and manages the lifecycle of per-server DHTTP endpoints built
//! on the shared [`Network`](h3x::dquic::Network).
//!
//! All mutating methods take `&self` and use interior mutability so that
//! `RootState` can be shared via `Arc` without external synchronization.

mod completion;
mod listener_registry;
pub(crate) mod owner;
mod process_ops;
mod server_ops;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
};

use dhttp::{ddns::publisher::CreatePublisherError, endpoint::Endpoint, name::DhttpName};
use h3x::{
    dquic::{Network, server::ServerQuicConfig},
    quic::Listen as _,
};
use nix::{
    sys::wait::WaitStatus,
    unistd::{Pid, Uid},
};
use snafu::{Report, Snafu};
use tokio::{
    sync::{Mutex, RwLock},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use crate::hypervisor::{
    endpoint_factory::BuildEndpointResolverError,
    task_scope::TaskScope,
    worker_handle::{WorkerHandle, WorkerHandleError},
};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Error returned when `acquire_listener` fails.
#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum AcquireListenerError {
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
        source: CreatePublisherError,
    },
    #[snafu(display("listener owner is not available"))]
    OwnerUnavailable,
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum ReleaseListenerError {
    #[snafu(display("listener is not owned by caller"))]
    NotOwner,
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum RebuildListenerError {
    #[snafu(display("listener is not owned by caller"))]
    NotOwner,
    #[snafu(display("server name conflicted"))]
    ConflictedName,
    #[snafu(display("failed to acquire replacement listener"))]
    Replacement { source: AcquireListenerError },
}

#[derive(Debug, Snafu)]
#[snafu(module, visibility(pub(crate)))]
pub enum WorkerStartupError {
    #[snafu(display("worker startup timed out"))]
    Timeout,
    #[snafu(display("failed to create mux channel from worker fd"))]
    MuxChannelFromFd { source: std::io::Error },
    #[snafu(display("failed to split worker mux channel"))]
    MuxChannelSplit {
        source: h3x::ipc::transport::SplitError,
    },
    #[snafu(display("failed to establish worker remoc transport"))]
    ConnectTransport {
        source: remoc::ConnectError<
            h3x::ipc::transport::MuxSinkError,
            h3x::ipc::transport::MuxStreamError,
        >,
    },
    #[snafu(display("failed to send worker bootstrap"))]
    SendBootstrap {
        source: remoc::rch::base::SendError<crate::ipc::WorkerBootstrap>,
    },
    #[snafu(display("failed to receive worker hello"))]
    ReceiveHello { source: remoc::rch::base::RecvError },
    #[snafu(display("worker closed channel without sending startup hello"))]
    MissingHello,
    #[snafu(display("worker was unregistered during startup"))]
    WorkerUnregistered,
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum WorkerProcessError {
    #[snafu(display("worker startup failed"))]
    Startup { source: WorkerStartupError },
    #[snafu(display("worker ipc disconnected"))]
    IpcDisconnected,
    #[snafu(display("worker exited with status {status:?}"))]
    ChildExited { status: WaitStatus },
    #[snafu(display("failed to poll worker status"))]
    PollStatus { source: WorkerHandleError },
    #[snafu(display("worker replaced by another process for the same uid"))]
    UidReplaced,
    #[snafu(display("worker removed by configuration reload"))]
    ReloadRemoved,
    #[snafu(display("worker changed by configuration reload"))]
    ReloadChanged,
    #[snafu(display("worker shutdown timed out"))]
    ShutdownTimeout,
    #[snafu(display("worker force-killed during shutdown"))]
    ForcedShutdown,
    #[snafu(display("worker stopped during root shutdown"))]
    RootShutdown,
}

impl WorkerProcessError {
    pub fn is_restartable(&self) -> bool {
        matches!(
            self,
            Self::Startup { .. }
                | Self::IpcDisconnected
                | Self::ChildExited { .. }
                | Self::PollStatus { .. }
        )
    }
}

#[derive(Debug)]
pub struct WorkerFailure {
    pub pid: Pid,
    pub error: WorkerProcessError,
}

pub(super) struct FailedWorkerRecord {
    pub(super) target: crate::config::ResolvedWorkerTarget,
    pub(super) reason: String,
}

/// Per-worker-process tracking record.
pub(super) struct WorkerProcessRecord {
    /// The UID this worker runs as.
    pub(super) uid: Uid,
    /// The username this worker runs as.
    pub(super) username: String,
    /// Set of server_names owned by this worker.
    pub(super) owned_servers: HashSet<DhttpName<'static>>,
    /// Structured task scope for root-side tasks owned by this worker.
    pub(super) tasks: TaskScope,
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
    /// uid → desired worker target from the current root configuration.
    pub(super) desired_workers: HashMap<Uid, crate::config::ResolvedWorkerTarget>,
    /// uid → failed worker target waiting for the next reload retry.
    pub(super) failed_workers: HashMap<Uid, FailedWorkerRecord>,
    /// Pending process-level failures reported by worker-scoped tasks.
    pub(super) worker_failures: VecDeque<WorkerFailure>,
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
    /// Listener entries (behind RwLock for concurrent reads).
    listeners: RwLock<listener_registry::ListenerRegistry<ListenerResource>>,
    /// Process/user bookkeeping (behind Mutex).
    pub(super) inner: Mutex<Inner>,
    /// Structured task scope for root-local service resources.
    pub(super) local_tasks: TaskScope,
    /// Notified when SIGCHLD arrives so the monitor loop wakes immediately.
    pub worker_notify: tokio::sync::Notify,
}

pub(super) struct ListenerResource {
    endpoint: Endpoint,
    shutdown_token: CancellationToken,
    publish_token: CancellationToken,
    publish_task: Option<JoinHandle<()>>,
}

impl ListenerResource {
    pub(super) fn new(
        endpoint: Endpoint,
        shutdown_token: CancellationToken,
        publish_token: CancellationToken,
        publish_task: Option<JoinHandle<()>>,
    ) -> Self {
        Self {
            endpoint,
            shutdown_token,
            publish_token,
            publish_task,
        }
    }

    pub(super) async fn destroy(mut self) {
        self.shutdown_token.cancel();
        self.publish_token.cancel();
        if let Some(task) = self.publish_task.take() {
            let _ = task.await;
        }
        if let Err(error) = self.endpoint.shutdown().await {
            tracing::warn!(
                error = %Report::from_error(&error),
                "failed to shut down listener resource"
            );
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
            listeners: RwLock::new(listener_registry::ListenerRegistry::new()),
            inner: Mutex::new(Inner {
                processes: HashMap::new(),
                users: HashMap::new(),
                desired_workers: HashMap::new(),
                failed_workers: HashMap::new(),
                worker_failures: VecDeque::new(),
            }),
            local_tasks: TaskScope::new(),
            worker_notify: tokio::sync::Notify::new(),
        }
    }
}

#[cfg(test)]
mod tests;
