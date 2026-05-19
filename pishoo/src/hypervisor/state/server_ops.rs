//! Server registry operations on [`RootState`].

use std::sync::Arc;

use dhttp::name::DhttpName;
use gateway::control_plane::ListenRequest;
use h3x::dquic::ServerBinding;
use snafu::IntoError;
use tokio::sync::mpsc;
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};
use tracing::Instrument;

use super::{RegisterError, RootState, ServerEntry, ServiceOwner, register_error};
use crate::listen::PerServerListener;

impl RootState {
    // -----------------------------------------------------------------------
    // Server registry
    // -----------------------------------------------------------------------

    /// Unified entry point for registering a listener.
    ///
    /// State machine:
    /// - **Vacant** → insert `Registering` sentinel → bind server on network
    ///   → promote to `Active`.
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
        let server_name =
            DhttpName::try_from_str_full(request.identity.name().as_full().to_owned())
                .expect("listen request identity must be a dhttp name");

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
                        _accept_task,
                        ..
                    }) = old
                    {
                        shutdown_token.cancel();
                        if let ServiceOwner::Worker(pid) = old_owner {
                            let mut inner = self.inner.lock().await;
                            if let Some(proc) = inner.processes.get_mut(&pid) {
                                proc.owned_servers.remove(&server_name);
                            }
                        }
                        // Abort the accept task, then await it so the old
                        // owner's `ServerBinding` is dropped before we
                        // return — otherwise a subsequent re-register of
                        // the same SNI (e.g. after `scrub_conflicts`)
                        // would race against the old binding and fail
                        // with `SniInUse`.
                        _accept_task.abort();
                        let _ = _accept_task.await;
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

        // Phase 2: name is claimed — convert Listen declarations to
        // BindPatterns, acquire BindHandles from the shared Network, then
        // register the SNI via bind_server.
        // No lock held — other server_names can be read/written concurrently.
        let bind_patterns = request
            .bind
            .iter()
            .flat_map(gateway::parse::Listens::to_bind_patterns)
            .collect::<Vec<_>>();
        let mut bind_handles = Vec::with_capacity(bind_patterns.len());
        for pattern in &bind_patterns {
            bind_handles.push(self.network.bind(pattern.clone()).await);
        }

        let identity = Arc::new(request.identity.clone());
        let server_binding = match self
            .network
            .bind_server(
                identity,
                self.server_qcfg.clone(),
                Arc::new(bind_patterns.clone()),
            )
            .await
        {
            Ok(binding) => binding,
            Err(source) => {
                // Rollback the sentinel. Dropping `bind_handles` releases the
                // bindings acquired above (no other server uses them).
                self.servers.write().await.entries.remove(&server_name);
                return Err(register_error::BindServerSnafu {
                    server_name: server_name.clone(),
                }
                .into_error(source));
            }
        };

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
                    // cleanup). Drop the server_binding to release the SNI.
                    drop(server_binding);
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
            let accept_task = spawn_accept_task(server_binding, tx.clone());

            // DNS publishing is moving to dhttp::Endpoint/DnsPublisher in
            // the next phase. Do not keep the legacy BindUri-based publisher
            // alive here, because this registry now stores BindPattern intent
            // rather than concrete BindUri snapshots.
            let publish_task = None;

            registry.entries.insert(
                server_name.clone(),
                ServerEntry::Active {
                    owner,
                    conn_tx: tx,
                    shutdown_token: shutdown_token.clone(),
                    bind_patterns,
                    bind_handles,
                    _accept_task: accept_task,
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
                    let retired = self.servers.write().await.retire_entry(&server_name);
                    if let Some(task) = retired {
                        let _ = task.await;
                    }
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
    pub async fn release_server(&self, server_name: &DhttpName<'static>, owner: ServiceOwner) {
        let retired = {
            let mut registry = self.servers.write().await;
            let owned = matches!(
                registry.entries.get(server_name),
                Some(ServerEntry::Active { owner: existing_owner, .. }) if *existing_owner == owner
            );
            if !owned {
                return;
            }
            registry.retire_entry(server_name)
        };

        // Wait for the fanout task to finish so its captured
        // `ServerBinding` is dropped (and the SNI slot freed) before we
        // return control to the caller.
        if let Some(task) = retired {
            let _ = task.await;
        }

        if let ServiceOwner::Worker(pid) = owner {
            let mut inner = self.inner.lock().await;
            if let Some(process) = inner.processes.get_mut(&pid) {
                process.owned_servers.remove(server_name);
            }
        }
    }

    /// Remove all `Conflicted` entries from the registry.
    ///
    /// Called during reload (SIGHUP) **before** forwarding the signal to
    /// workers, so that workers can re-register previously-conflicted names.
    pub async fn scrub_conflicts(&self) -> Vec<DhttpName<'static>> {
        let mut registry = self.servers.write().await;
        let conflicted: Vec<DhttpName<'static>> = registry
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
}

/// Spawn the per-server fanout task that drains the shared [`ServerBinding`]
/// mpmc queue into the worker-visible mpsc channel.
///
/// The task terminates inherently when either end of the pipe closes:
/// dropping every [`ServerBinding`] clone yields `None` from
/// [`ServerBinding::recv`]; closing the worker's `mpsc::Receiver` (by
/// dropping [`PerServerListener`]) makes `tx.send` fail. The returned
/// [`AbortOnDropHandle`] additionally guarantees cleanup on retire.
fn spawn_accept_task(
    binding: ServerBinding,
    tx: mpsc::Sender<Arc<h3x::dquic::prelude::Connection>>,
) -> AbortOnDropHandle<()> {
    let name = binding.name().clone();
    AbortOnDropHandle::new(tokio::spawn(
        async move {
            // loop exits when `binding.recv()` returns None (SNI dropped)
            // or when the receiver side of `tx` is closed.
            loop {
                match binding.recv().await {
                    Some(conn) => {
                        if tx.send(conn).await.is_err() {
                            tracing::debug!(%name, "accept task: receiver dropped, exiting");
                            break;
                        }
                    }
                    None => {
                        tracing::debug!(%name, "accept task: binding closed, exiting");
                        break;
                    }
                }
            }
        }
        .in_current_span(),
    ))
}
