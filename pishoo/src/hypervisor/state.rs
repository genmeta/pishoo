//! Root-side ownership registry for server_name → local/worker mappings.
//!
//! Tracks which worker process owns which server names, provides conflict
//! detection, and manages the lifecycle of per-server DHTTP endpoints built
//! on the shared [`Network`](dhttp::h3x::dquic::Network).
//!
//! All mutating methods take `&self` and use interior mutability so that
//! `RootState` can be shared via `Arc` without external synchronization.

mod listener_registry;
pub(crate) mod owner;
mod process_ops;
mod server_ops;

#[cfg(test)]
use std::sync::Arc;
use std::{
    collections::{HashMap, VecDeque},
    future::Future,
};

use dhttp::{
    endpoint::{CreateEndpointPublicationLoopError, Endpoint},
    h3x::quic::Listen as _,
    network::DhttpNetwork,
};
use nix::{
    sys::wait::WaitStatus,
    unistd::{Pid, Uid},
};
use snafu::{Report, Snafu};
use tokio::sync::{Mutex, RwLock};
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};

use crate::hypervisor::{
    endpoint_factory::BuildRegisteredEndpointError,
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
    #[snafu(display("failed to build registered endpoint"))]
    BuildEndpoint {
        source: BuildRegisteredEndpointError,
    },
    #[snafu(display("failed to create dns publication loop for registered endpoint"))]
    CreatePublisher {
        source: CreateEndpointPublicationLoopError,
    },
    #[snafu(display("registered endpoint has no dns publishers"))]
    MissingPublisher,
    #[snafu(display("listener owner is not available"))]
    OwnerUnavailable,
    #[snafu(display("listener transition stopped before result delivery"))]
    TransitionStopped,
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
    #[snafu(display("listener transition stopped before result delivery"))]
    TransitionStopped,
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
        source: dhttp::h3x::ipc::transport::SplitError,
    },
    #[snafu(display("failed to establish worker remoc transport"))]
    ConnectTransport {
        source: remoc::ConnectError<
            dhttp::h3x::ipc::transport::MuxSinkError,
            dhttp::h3x::ipc::transport::MuxStreamError,
        >,
    },
    #[snafu(display("failed to send worker bootstrap"))]
    SendBootstrap {
        #[snafu(source(from(
            remoc::rch::base::SendError<crate::ipc::WorkerBootstrap>,
            Box::new
        )))]
        source: Box<remoc::rch::base::SendError<crate::ipc::WorkerBootstrap>>,
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
/// Tracks `server_name → owner` and `uid → pid` mappings. Owns the shared
/// [`DhttpNetwork`] used for every server registration, and coordinates all
/// server registration / cleanup.
pub struct RootState {
    /// Shared DHTTP network with installed SNI dispatcher and DNS keepalives.
    pub network: DhttpNetwork,
    /// Listener entries (behind RwLock for concurrent reads).
    listeners: RwLock<listener_registry::ListenerRegistry<ListenerResource>>,
    /// Process/user bookkeeping (behind Mutex).
    pub(super) inner: Mutex<Inner>,
    /// Structured task scope for in-process service resources.
    pub(super) local_tasks: TaskScope,
    /// Root-owned async resource transitions that must finish even if the
    /// requesting RPC/reload future is cancelled.
    pub(super) resource_tasks: TaskScope,
    /// Notified when SIGCHLD arrives so the monitor loop wakes immediately.
    pub worker_notify: tokio::sync::Notify,
    #[cfg(test)]
    listener_test_hooks: ListenerTestHooks,
}

pub(super) struct ListenerResource {
    endpoint: Endpoint,
    shutdown_token: CancellationToken,
    publish_token: CancellationToken,
    publish_task: Option<AbortOnDropHandle<()>>,
}

impl ListenerResource {
    pub(super) fn new(
        endpoint: Endpoint,
        shutdown_token: CancellationToken,
        publish_token: CancellationToken,
        publish_task: Option<AbortOnDropHandle<()>>,
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
    /// Create a new root state with the given shared [`DhttpNetwork`].
    pub fn new(network: DhttpNetwork) -> Self {
        Self {
            network,
            listeners: RwLock::new(listener_registry::ListenerRegistry::new()),
            inner: Mutex::new(Inner {
                processes: HashMap::new(),
                users: HashMap::new(),
                desired_workers: HashMap::new(),
                failed_workers: HashMap::new(),
                worker_failures: VecDeque::new(),
            }),
            local_tasks: TaskScope::new(),
            resource_tasks: TaskScope::new(),
            worker_notify: tokio::sync::Notify::new(),
            #[cfg(test)]
            listener_test_hooks: ListenerTestHooks::default(),
        }
    }

    pub(super) fn spawn_resource_transition<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.resource_tasks.spawn(|_| future);
    }

    pub async fn wait_resource_transitions(&self) {
        self.resource_tasks.cancel_and_wait().await;
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
#[derive(Debug, Default)]
pub(super) struct ListenerTestHooks {
    next_destroy: std::sync::Mutex<Option<ListenerPause>>,
    next_delivery: std::sync::Mutex<Option<ListenerPause>>,
}

#[cfg(test)]
#[derive(Debug, Clone)]
pub(crate) struct ListenerPause {
    inner: Arc<ListenerPauseInner>,
}

#[cfg(test)]
#[derive(Debug)]
struct ListenerPauseInner {
    started: tokio::sync::Notify,
    resume: tokio::sync::Notify,
    resumed: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
impl ListenerPause {
    fn new() -> Self {
        Self {
            inner: Arc::new(ListenerPauseInner {
                started: tokio::sync::Notify::new(),
                resume: tokio::sync::Notify::new(),
                resumed: std::sync::atomic::AtomicBool::new(false),
            }),
        }
    }

    pub(crate) async fn wait_started(&self) {
        self.inner.started.notified().await;
    }

    pub(crate) fn resume(&self) {
        self.inner
            .resumed
            .store(true, std::sync::atomic::Ordering::Release);
        self.inner.resume.notify_waiters();
    }

    pub(super) async fn pause(&self) {
        self.inner.started.notify_waiters();
        loop {
            if self
                .inner
                .resumed
                .load(std::sync::atomic::Ordering::Acquire)
            {
                return;
            }
            let notified = self.inner.resume.notified();
            if self
                .inner
                .resumed
                .load(std::sync::atomic::Ordering::Acquire)
            {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
impl ListenerTestHooks {
    fn set_next_destroy(&self) -> ListenerPause {
        let pause = ListenerPause::new();
        let mut next = self
            .next_destroy
            .lock()
            .expect("listener destroy hook should not be poisoned");
        *next = Some(pause.clone());
        pause
    }

    fn take_next_destroy(&self) -> Option<ListenerPause> {
        self.next_destroy
            .lock()
            .expect("listener destroy hook should not be poisoned")
            .take()
    }

    fn set_next_delivery(&self) -> ListenerPause {
        let pause = ListenerPause::new();
        let mut next = self
            .next_delivery
            .lock()
            .expect("listener delivery hook should not be poisoned");
        *next = Some(pause.clone());
        pause
    }

    fn take_next_delivery(&self) -> Option<ListenerPause> {
        self.next_delivery
            .lock()
            .expect("listener delivery hook should not be poisoned")
            .take()
    }
}
