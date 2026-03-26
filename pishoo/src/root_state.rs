//! Root-side ownership registry for server_name → local/worker mappings.
//!
//! Tracks which worker process owns which server names, provides conflict
//! detection (first-come-first-served), and manages the lifecycle of
//! per-server listen adapters routed from the central `QuicListeners`.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use nix::{
    sys::{signal::Signal, wait::WaitStatus},
    unistd::{Pid, Uid},
};
use remoc::prelude::ServerShared;
use snafu::{FromString, Report, Whatever};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::{
    per_server_listen::PerServerListenAdapter,
    protocol::{
        ListenRequestError, OpenConnector, OpenConnectorError, ReleaseListen, ReleaseListenError,
    },
    remoc_bridge::{
        ConnectorHandle, ConnectorServerShared, ListenerHandle, ListenerServerShared,
        ServedConnector, ServedListener,
    },
    tls,
    worker_spawn::WorkerHandle,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceOwner {
    Local,
    Worker(Pid),
}

/// Per-server ownership record stored in the root registry.
pub struct ServerRecord {
    /// Owner kind of this server_name.
    pub owner: ServiceOwner,
    /// Sender for routing connections from the central accept loop to this
    /// server's [`PerServerListenAdapter`].
    pub conn_tx: mpsc::Sender<gm_quic::prelude::Connection>,
    /// Shutdown token for the [`PerServerListenAdapter`].
    pub shutdown_token: CancellationToken,
}

/// Per-worker-process tracking record.
pub struct WorkerProcessRecord {
    /// The UID this worker runs as.
    pub uid: Uid,
    /// Set of server_names owned by this worker.
    pub owned_servers: HashSet<String>,
    /// Handle to the spawned worker process.
    pub worker_handle: WorkerHandle,
    /// Cancellation tokens for connector serve futures owned by this worker.
    pub connector_shutdown_tokens: Vec<CancellationToken>,
}

/// Root-side ownership registry.
///
/// Tracks `server_name → pid`, `pid → owned_servers`, and `uid → pid`
/// mappings. Provides the core logic for `request_listen`, `release_listen`,
/// and worker lifecycle management.
pub struct RootState {
    /// The shared QUIC listeners object.
    pub listeners: Arc<gm_quic::prelude::QuicListeners>,
    /// Shared QUIC client for creating outbound connectors.
    pub quic_client: Arc<gm_quic::prelude::QuicClient>,
    /// server_name → ownership + routing sender.
    servers: HashMap<String, ServerRecord>,
    /// pid → worker process info.
    processes: HashMap<Pid, WorkerProcessRecord>,
    /// uid → pid mapping.
    users: HashMap<Uid, Pid>,
}

#[derive(Debug, Clone)]
pub struct CleanupSummary {
    pub pid: Pid,
    pub uid: Uid,
    pub servers_cleaned: usize,
    pub connectors_cleaned: usize,
}

impl RootState {
    /// Create a new root state with the given shared QUIC listeners.
    pub fn new(
        listeners: Arc<gm_quic::prelude::QuicListeners>,
        quic_client: Arc<gm_quic::prelude::QuicClient>,
    ) -> Self {
        Self {
            listeners,
            quic_client,
            servers: HashMap::new(),
            processes: HashMap::new(),
            users: HashMap::new(),
        }
    }

    /// Register a new worker process in the registry.
    ///
    /// Called after successfully spawning a worker via `spawn_worker`.
    pub fn register_worker(&mut self, pid: Pid, uid: Uid, worker_handle: WorkerHandle) {
        if let Some(old_pid) = self.users.get(&uid).copied()
            && old_pid != pid
        {
            self.cleanup_worker_with_reason(old_pid, "uid_replaced");
        }
        self.processes.insert(
            pid,
            WorkerProcessRecord {
                uid,
                owned_servers: HashSet::new(),
                worker_handle,
                connector_shutdown_tokens: Vec::new(),
            },
        );
        self.users.insert(uid, pid);
        tracing::info!(pid = %pid, uid = uid.as_raw(), "registered worker");
    }

    /// Remove all resources for a dead/exited worker process.
    ///
    /// Cleans up all owned server_names (removes from `QuicListeners`,
    /// cancels adapters, drops routing senders), and removes the worker from
    /// the `users` map.
    pub fn cleanup_worker(&mut self, pid: Pid) {
        self.cleanup_worker_with_reason(pid, "cleanup");
    }

    pub fn cleanup_worker_with_reason(&mut self, pid: Pid, reason: &str) -> Option<CleanupSummary> {
        let Some(record) = self.processes.remove(&pid) else {
            tracing::debug!(pid = %pid, %reason, "cleanup skipped: worker not found");
            return None;
        };

        if self.users.get(&record.uid).copied() == Some(pid) {
            self.users.remove(&record.uid);
        }

        let mut servers_cleaned = 0usize;

        for server_name in &record.owned_servers {
            if self.retire_server(server_name).is_some() {
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
            "worker cleanup complete"
        );
        Some(summary)
    }

    fn retire_server(&mut self, server_name: &str) -> Option<()> {
        let server_record = self.servers.remove(server_name)?;
        server_record.shutdown_token.cancel();
        self.listeners.remove_server(server_name);
        Some(())
    }

    pub fn retire_local_servers(&mut self) -> Vec<String> {
        let local_server_names = self
            .servers
            .iter()
            .filter_map(|(server_name, record)| {
                (record.owner == ServiceOwner::Local).then_some(server_name.clone())
            })
            .collect::<Vec<_>>();

        for server_name in &local_server_names {
            let _ = self.retire_server(server_name);
        }

        local_server_names
    }

    pub async fn register_local_server(
        &mut self,
        server_name: String,
        bind: Vec<String>,
        cert_pem: &[u8],
        key_pem: &[u8],
        conn_tx: mpsc::Sender<gm_quic::prelude::Connection>,
        shutdown_token: CancellationToken,
    ) -> Result<(), Whatever> {
        if self.servers.contains_key(&server_name) {
            snafu::whatever!("server `{server_name}` conflicts with an existing listener");
        }

        let (certs, key) = tls::validate_tls_material(cert_pem, key_pem).map_err(|error| {
            Whatever::with_source(Box::new(error), "invalid local tls material".to_string())
        })?;

        self.listeners
            .add_server(&server_name, certs.as_slice(), &key, bind, None::<Vec<u8>>)
            .await
            .map_err(|error| {
                Whatever::with_source(
                    Box::new(error),
                    format!("failed to add local server `{server_name}` to listeners"),
                )
            })?;

        self.servers.insert(
            server_name,
            ServerRecord {
                owner: ServiceOwner::Local,
                conn_tx,
                shutdown_token,
            },
        );
        Ok(())
    }

    /// Validate a listen request under the lock (fast, no I/O).
    ///
    /// Returns a cloned `Arc<QuicListeners>` so the caller can call
    /// [`QuicListeners::add_server`] **outside** the mutex.
    pub fn validate_listen_request(
        &self,
        caller_pid: Pid,
        server_name: &str,
    ) -> Result<Arc<gm_quic::prelude::QuicListeners>, ListenRequestError> {
        if self.servers.contains_key(server_name) {
            tracing::warn!(caller_pid = %caller_pid, %server_name, "request_listen conflict");
            return Err(ListenRequestError::Conflict);
        }
        if !self.processes.contains_key(&caller_pid) {
            return Err(ListenRequestError::Internal {
                message: format!("unknown caller pid {caller_pid}"),
            });
        }
        Ok(self.listeners.clone())
    }

    /// Commit a validated listen request after `add_server` has completed.
    ///
    /// Creates the per-server listen adapter, spawns the RTC serve future,
    /// and records the server in the registry.  Must be called with the lock
    /// held.  Re-checks for conflicts that could race during `add_server`.
    pub fn commit_listen_request(
        &mut self,
        caller_pid: Pid,
        server_name: String,
    ) -> Result<ListenerHandle, ListenRequestError> {
        // Re-check: another request may have raced while we were in add_server.
        if self.servers.contains_key(&server_name) {
            self.listeners.remove_server(&server_name);
            tracing::warn!(
                caller_pid = %caller_pid,
                %server_name,
                "request_listen conflict (raced during add_server)"
            );
            return Err(ListenRequestError::Conflict);
        }
        if !self.processes.contains_key(&caller_pid) {
            self.listeners.remove_server(&server_name);
            return Err(ListenRequestError::Internal {
                message: format!("unknown caller pid {caller_pid}"),
            });
        }

        let (tx, rx) = mpsc::channel(128);
        let shutdown_token = CancellationToken::new();
        let adapter = PerServerListenAdapter::new(rx, shutdown_token.clone());

        let (server, client) = ListenerServerShared::new(Arc::new(ServedListener::new(adapter)), 1);
        let serve_fut = async move {
            let _ = server.serve(true).await;
        };
        tokio::spawn(serve_fut.in_current_span());

        self.servers.insert(
            server_name.clone(),
            ServerRecord {
                owner: ServiceOwner::Worker(caller_pid),
                conn_tx: tx,
                shutdown_token,
            },
        );

        if let Some(process) = self.processes.get_mut(&caller_pid) {
            process.owned_servers.insert(server_name);
        }

        tracing::info!(caller_pid = %caller_pid, "request_listen success");
        Ok(ListenerHandle::new(client))
    }

    /// Handle an `open_connector` call from a worker.
    ///
    /// connector root-owned: root 创建 connector 并统一托管生命周期，worker 只消费 handle。
    /// Creates a [`LocalQuicConnector`] wrapping the shared [`QuicClient`],
    /// converts it to a [`RemoteQuicConnector`] via RTC, spawns the serve
    /// future (with a cancellation token for cleanup), and tracks it per-pid.
    pub async fn open_connector(
        &mut self,
        caller_pid: Pid,
        _request: OpenConnector,
    ) -> Result<ConnectorHandle, OpenConnectorError> {
        // Verify caller is a registered worker.
        if !self.processes.contains_key(&caller_pid) {
            return Err(OpenConnectorError::Internal {
                message: format!("unknown caller pid {caller_pid}"),
            });
        }

        let (server, client) =
            ConnectorServerShared::new(Arc::new(ServedConnector::new(self.quic_client.clone())), 1);
        let serve_fut = async move {
            let _ = server.serve(true).await;
        };

        // Create a cancellation token so we can stop the serve future on cleanup.
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        tokio::spawn(
            async move {
                tokio::select! {
                    () = serve_fut => {}
                    () = cancel_clone.cancelled() => {}
                }
            }
            .in_current_span(),
        );

        // Track the cancellation token in the worker's process record.
        if let Some(process) = self.processes.get_mut(&caller_pid) {
            process.connector_shutdown_tokens.push(cancel);
        }

        tracing::info!(caller_pid = %caller_pid, "open_connector success");

        Ok(ConnectorHandle::new(client))
    }

    /// Handle a `release_listen` call from a worker.
    ///
    /// Verifies ownership, cancels the per-server adapter, removes the server
    /// from `QuicListeners`, and cleans up registry maps.
    pub fn release_listen(
        &mut self,
        caller_pid: Pid,
        request: ReleaseListen,
    ) -> Result<(), ReleaseListenError> {
        let server_name = request.server_name.as_full();

        // Check the server exists.
        let Some(server_record) = self.servers.get(server_name) else {
            return Err(ReleaseListenError::NotFound);
        };

        // Ownership check.
        if server_record.owner != ServiceOwner::Worker(caller_pid) {
            tracing::warn!(caller_pid = %caller_pid, %server_name, "release_listen not owner");
            return Err(ReleaseListenError::NotOwner);
        }

        // Remove from servers map.
        self.retire_server(server_name)
            .expect("server must exist after ownership check");

        // Remove from process record.
        if let Some(process) = self.processes.get_mut(&caller_pid) {
            process.owned_servers.remove(server_name);
        }

        tracing::info!(caller_pid = %caller_pid, %server_name, "release_listen success");

        Ok(())
    }

    pub fn collect_exited_workers(&mut self) -> Vec<Pid> {
        let mut exited = Vec::new();
        for (pid, process) in &mut self.processes {
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
                    tracing::error!(pid = %pid, error = %Report::from_error(&error), "failed to poll worker status");
                    exited.push(*pid);
                }
            }
        }
        exited
    }

    pub fn force_kill_workers(&mut self, reason: &str) -> Vec<Pid> {
        let mut killed = Vec::new();
        for (pid, process) in &mut self.processes {
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

    /// Look up the routing sender for a given server_name.
    ///
    /// Used by the central accept loop to route connections to the correct
    /// per-server adapter.
    pub fn get_conn_sender(
        &self,
        server_name: &str,
    ) -> Option<mpsc::Sender<gm_quic::prelude::Connection>> {
        self.servers.get(server_name).map(|r| r.conn_tx.clone())
    }

    pub fn server_owner(&self, server_name: &str) -> Option<ServiceOwner> {
        self.servers.get(server_name).map(|record| record.owner)
    }

    /// Get the PID for a given UID, if a worker is registered.
    pub fn get_pid_for_uid(&self, uid: Uid) -> Option<Pid> {
        self.users.get(&uid).copied()
    }

    /// Get a reference to a worker process record.
    pub fn get_process(&self, pid: Pid) -> Option<&WorkerProcessRecord> {
        self.processes.get(&pid)
    }

    pub fn worker_pids(&self) -> Vec<Pid> {
        self.processes.keys().copied().collect()
    }

    pub fn forward_unix_signal(&mut self, signal: Signal) {
        for (pid, record) in &mut self.processes {
            let Some(raw_pid) = record.worker_handle.pid() else {
                continue;
            };
            let child_pid = Pid::from_raw(raw_pid as i32);
            if let Err(error) = nix::sys::signal::kill(child_pid, signal) {
                tracing::warn!(pid = %pid, error = %Report::from_error(&error), ?signal, "failed to forward unix signal to worker");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use super::*;

    struct SharedQuicFixture {
        listeners: Arc<gm_quic::prelude::QuicListeners>,
        client: Arc<gm_quic::prelude::QuicClient>,
    }

    fn shared_quic_fixture() -> &'static SharedQuicFixture {
        static FIXTURE: OnceLock<SharedQuicFixture> = OnceLock::new();
        FIXTURE.get_or_init(|| {
            let roots = crate::tls::root_cert_store();
            let listeners = gm_quic::prelude::QuicListeners::builder()
                .with_parameters(gm_quic::prelude::handy::server_parameters())
                .with_client_cert_verifier(
                    rustls::server::WebPkiClientVerifier::builder(roots)
                        .allow_unauthenticated()
                        .build()
                        .expect("build verifier"),
                )
                .with_alpns([b"h3".as_slice()])
                .listen(16)
                .expect("create listeners");
            let client = Arc::new(
                gm_quic::prelude::QuicClient::builder()
                    .with_root_certificates(Arc::new(rustls::RootCertStore::empty()))
                    .without_cert()
                    .with_alpns(vec!["h3"])
                    .build(),
            );

            SharedQuicFixture { listeners, client }
        })
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cleanup_only_removes_uid_mapping_if_pid_matches() {
        let fixture = shared_quic_fixture();
        let mut state = RootState::new(fixture.listeners.clone(), fixture.client.clone());

        state.users.insert(Uid::from_raw(1000), Pid::from_raw(200));
        state.processes.insert(
            Pid::from_raw(100),
            WorkerProcessRecord {
                uid: Uid::from_raw(1000),
                owned_servers: HashSet::new(),
                worker_handle: super::WorkerHandle::new(
                    tokio::process::Command::new("/usr/bin/true")
                        .spawn()
                        .expect("spawn child"),
                ),
                connector_shutdown_tokens: Vec::new(),
            },
        );

        let summary = state
            .cleanup_worker_with_reason(Pid::from_raw(100), "test")
            .expect("cleanup summary");
        assert_eq!(summary.pid, Pid::from_raw(100));
        assert_eq!(
            state.get_pid_for_uid(Uid::from_raw(1000)),
            Some(Pid::from_raw(200))
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn force_kill_workers_terminates_registered_children() {
        let fixture = shared_quic_fixture();
        let mut state = RootState::new(fixture.listeners.clone(), fixture.client.clone());
        let pid = Pid::from_raw(300);
        let uid = Uid::from_raw(4000);

        let child = tokio::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("spawn child");
        state.register_worker(pid, uid, super::WorkerHandle::new(child));

        let killed = state.force_kill_workers("test_force_kill");
        assert_eq!(killed, vec![pid]);

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let exited = state.collect_exited_workers();
            if exited.contains(&pid) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "force-killed worker should exit promptly"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }

        let summary = state
            .cleanup_worker_with_reason(pid, "test_force_kill_cleanup")
            .expect("cleanup summary");
        assert_eq!(summary.pid, pid);
    }
}
