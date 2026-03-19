//! Root-side ownership registry for server_name → worker process mappings.
//!
//! Tracks which worker process owns which server names, provides conflict
//! detection (first-come-first-served), and manages the lifecycle of
//! per-server listen adapters routed from the central `QuicListeners`.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use genmeta_home::identity::Name;
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
        RequestListen,
    },
    remoc_bridge::{
        ConnectorHandle, ConnectorServerShared, ListenerHandle, ListenerServerShared,
        ServedConnector, ServedListener,
    },
    worker_spawn::WorkerHandle,
};

/// Per-server ownership record stored in the root registry.
pub struct ServerRecord {
    /// PID of the worker process that owns this server_name.
    pub owner_pid: Pid,
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
    pub owned_servers: HashSet<Name<'static>>,
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
    servers: HashMap<Name<'static>, ServerRecord>,
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

    fn retire_server(&mut self, server_name: &Name<'static>) -> Option<()> {
        let server_record = self.servers.remove(server_name)?;
        server_record.shutdown_token.cancel();
        self.listeners.remove_server(server_name.as_full());
        Some(())
    }

    /// Handle a `request_listen` call from a worker.
    ///
    /// Performs conflict detection (first-come-first-served), reads TLS cert/key
    /// files, registers the server with `QuicListeners`, creates a
    /// [`PerServerListenAdapter`], and wraps it via
    /// [`LocalQuicListener::into_remote()`] to produce a [`RemoteQuicListener`]
    /// for the worker.
    pub async fn request_listen(
        &mut self,
        caller_pid: Pid,
        request: RequestListen,
    ) -> Result<ListenerHandle, ListenRequestError> {
        let server_name = request.name;

        // 1. Conflict check: first-come-first-served.
        if self.servers.contains_key(&server_name) {
            tracing::warn!(caller_pid = %caller_pid, %server_name, "request_listen conflict");
            return Err(ListenRequestError::Conflict);
        }

        // 2. Verify caller is a registered worker.
        if !self.processes.contains_key(&caller_pid) {
            return Err(ListenRequestError::Internal {
                message: format!("unknown caller pid {caller_pid}"),
            });
        }

        // TODO: mataining bindings on network changed.
        // 4. Add server to QuicListeners.
        //    bind_uris is Vec<String>; BindUri implements From<String>.
        self.listeners
            .add_server(
                server_name.as_full(),
                request.certs.as_slice(),
                &request.key,
                request.bind,
                None::<Vec<u8>>,
            )
            .await
            .map_err(|error| {
                tracing::warn!(
                    caller_pid = %caller_pid,
                    %server_name,
                    error = %Report::from_error(&error),
                    "failed to add server to listeners"
                );
                ListenRequestError::Internal {
                    message: Report::from_error(Whatever::with_source(
                        Box::new(error),
                        format!("failed to add server `{server_name}`"),
                    ))
                    .to_string(),
                }
            })?;

        // 5. Create mpsc channel for routing connections to this server.
        let (tx, rx) = mpsc::channel(128);

        // 6. Create per-server listen adapter.
        let shutdown_token = CancellationToken::new();
        let adapter = PerServerListenAdapter::new(rx, shutdown_token.clone());

        let (server, client) = ListenerServerShared::new(Arc::new(ServedListener::new(adapter)), 1);
        let serve_fut = async move {
            let _ = server.serve(true).await;
        };

        // 8. Spawn the serve future to drive the RTC server.
        tokio::spawn(serve_fut.in_current_span());

        // 9. Update registry: server record.
        self.servers.insert(
            server_name.clone(),
            ServerRecord {
                owner_pid: caller_pid,
                conn_tx: tx,
                shutdown_token,
            },
        );

        // 10. Update process record: add to owned_servers set.
        if let Some(process) = self.processes.get_mut(&caller_pid) {
            process.owned_servers.insert(server_name);
        }

        tracing::info!(caller_pid = %caller_pid, "request_listen success");

        // 11. Return the RemoteQuicListener for the worker.
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
        let server_name = &request.server_name;

        // Check the server exists.
        let Some(server_record) = self.servers.get(server_name) else {
            return Err(ReleaseListenError::NotFound);
        };

        // Ownership check.
        if server_record.owner_pid != caller_pid {
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

    /// Look up the routing sender for a given server_name.
    ///
    /// Used by the central accept loop to route connections to the correct
    /// per-server adapter.
    pub fn get_conn_sender(
        &self,
        server_name: &Name<'static>,
    ) -> Option<&mpsc::Sender<gm_quic::prelude::Connection>> {
        self.servers.get(server_name).map(|r| &r.conn_tx)
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
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn cleanup_only_removes_uid_mapping_if_pid_matches() {
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
        let client = std::sync::Arc::new(
            gm_quic::prelude::QuicClient::builder()
                .with_root_certificates(std::sync::Arc::new(rustls::RootCertStore::empty()))
                .without_cert()
                .with_alpns(vec!["h3"])
                .build(),
        );
        let mut state = RootState::new(listeners, client);

        state.users.insert(Uid::from_raw(1000), Pid::from_raw(200));
        state.processes.insert(
            Pid::from_raw(100),
            WorkerProcessRecord {
                uid: Uid::from_raw(1000),
                owned_servers: HashSet::new(),
                worker_handle: super::WorkerHandle::new(
                    tokio::process::Command::new("/bin/true")
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
}
