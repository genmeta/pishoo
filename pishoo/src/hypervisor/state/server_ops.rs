//! Server registry operations on [`RootState`].

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Weak},
};

use gateway::control_plane::ListenRequest;
use h3x::{
    dquic::{prelude::BindUri, qinterface::BindInterface},
    endpoint::identity::NamedIdentity,
};
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

        // Phase 2: name is claimed — resolve bind URIs, acquire BindInterfaces
        // from the shared Network, then register the SNI via bind_server.
        // No lock held — other server_names can be read/written concurrently.
        let device_names = h3x::dquic::qinterface::device::Devices::global()
            .interfaces()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let bind_uri_strings = crate::listen::resolve_bind_uris(&request.bind, &device_names);
        let bound_ifaces = bind_uris_on_network(&self.network, &bind_uri_strings).await;

        let named_identity = Arc::new(NamedIdentity {
            name: Arc::from(server_name.as_str()),
            certs: request.identity.certs().to_vec(),
            key: Arc::new(request.identity.key().clone_key()),
        });
        let server_binding = match self
            .network
            .bind_server(named_identity, self.server_qcfg.clone())
            .await
        {
            Ok(binding) => binding,
            Err(source) => {
                // Rollback the sentinel. Dropping `bound_ifaces` releases the
                // BindInterfaces acquired above (no other server uses them).
                self.servers.write().await.entries.remove(&server_name);
                return Err(register_error::BindServerSnafu { server_name }.into_error(source));
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

            // Spawn per-server DNS publish task.
            let publish_config = gateway::dns::build_publish_config_from_identity(
                &request.identity,
                request.dns_resolver_url.as_deref(),
            );
            let publish_task = if publish_config.resolvers.is_empty() {
                None
            } else {
                let provider = bind_uri_provider(Arc::downgrade(self), server_name.clone());
                Some(gateway::dns::spawn_server_publish_task(
                    server_name.clone(),
                    publish_config,
                    self.network.clone(),
                    provider,
                ))
            };

            registry.entries.insert(
                server_name.clone(),
                ServerEntry::Active {
                    owner,
                    conn_tx: tx,
                    shutdown_token: shutdown_token.clone(),
                    listens: request.bind,
                    bound_ifaces,
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
    pub async fn release_server(&self, server_name: &str, owner: ServiceOwner) {
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

    /// Reconcile bind URIs for active servers affected by a network
    /// interface event.
    ///
    /// Only servers whose [`Listens`] reference the changed device (via
    /// [`IfaceRange::All`] or [`IfaceRange::Exact`]) are re-resolved.
    /// Listens with `specific_addrs` are always skipped (they don't depend
    /// on network interfaces).
    pub async fn reconcile_binds(&self, event: &h3x::dquic::qinterface::device::InterfaceEvent) {
        let device = event.device();

        let device_names: Vec<String> = h3x::dquic::qinterface::device::Devices::global()
            .interfaces()
            .keys()
            .cloned()
            .collect();

        // collect only servers whose listens are affected by this device.
        let affected: Vec<(String, Vec<gateway::parse::Listens>)> = {
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

        for (server_name, listens) in affected {
            let desired_uris: Vec<String> =
                crate::listen::resolve_bind_uris(&listens, &device_names);
            let desired_by_key: HashMap<String, BindUri> = desired_uris
                .iter()
                .map(|s| {
                    let uri = BindUri::from(s.as_str());
                    (uri.identity_key(), uri)
                })
                .collect();

            // Snapshot the currently-bound URIs under a read lock, then
            // compute the diff.
            let current_by_key: HashMap<String, BindUri> = {
                let registry = self.servers.read().await;
                match registry.entries.get(&server_name) {
                    Some(ServerEntry::Active { bound_ifaces, .. }) => bound_ifaces
                        .keys()
                        .map(|uri| (uri.identity_key(), uri.clone()))
                        .collect(),
                    _ => continue,
                }
            };

            let desired_keys: HashSet<&String> = desired_by_key.keys().collect();
            let current_keys: HashSet<&String> = current_by_key.keys().collect();

            let to_add: Vec<BindUri> = desired_keys
                .difference(&current_keys)
                .filter_map(|k| desired_by_key.get(*k).cloned())
                .collect();
            let to_remove: Vec<BindUri> = current_keys
                .difference(&desired_keys)
                .filter_map(|k| current_by_key.get(*k).cloned())
                .collect();

            if to_add.is_empty() && to_remove.is_empty() {
                continue;
            }

            // Acquire new BindInterfaces outside the registry lock.
            let mut added_ifaces: HashMap<BindUri, BindInterface> =
                HashMap::with_capacity(to_add.len());
            for uri in &to_add {
                let iface = self.network.bind(uri.clone()).await;
                added_ifaces.insert(uri.clone(), iface);
            }

            // Apply the diff to the live entry under the write lock.
            let mut registry = self.servers.write().await;
            match registry.entries.get_mut(&server_name) {
                Some(ServerEntry::Active { bound_ifaces, .. }) => {
                    for (uri, iface) in added_ifaces {
                        bound_ifaces.insert(uri, iface);
                    }
                    for uri in &to_remove {
                        bound_ifaces.remove(uri);
                    }
                }
                _ => continue,
            }
            drop(registry);

            if !to_add.is_empty() {
                let added: Vec<String> = to_add.iter().map(ToString::to_string).collect();
                tracing::info!(%server_name, added = ?added, "reconcile: binding new interfaces");
            }
            if !to_remove.is_empty() {
                let removed: Vec<String> = to_remove.iter().map(ToString::to_string).collect();
                tracing::info!(%server_name, removed = ?removed, "reconcile: unbound removed interfaces");
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
}

/// Acquire a [`BindInterface`] for each requested URI from the shared
/// [`Network`]. Duplicate URIs and any that fail to parse as a valid
/// [`BindUri`] (h3x panics at parse time, so this is best-effort) are
/// silently deduplicated on the [`BindUri::identity_key`] level.
async fn bind_uris_on_network(
    network: &Arc<h3x::endpoint::network::Network>,
    uri_strings: &[String],
) -> HashMap<BindUri, BindInterface> {
    let mut result = HashMap::with_capacity(uri_strings.len());
    for s in uri_strings {
        let uri = BindUri::from(s.as_str());
        // identity_key folds transient query params; keep one bind per key
        // to match h3x's own deduplication in `Binds::to_bind_uris`.
        if result.contains_key(&uri) {
            continue;
        }
        let iface = network.bind(uri.clone()).await;
        result.insert(uri, iface);
    }
    result
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
    binding: h3x::endpoint::network::ServerBinding,
    tx: mpsc::Sender<Arc<h3x::dquic::prelude::Connection>>,
) -> AbortOnDropHandle<()> {
    let name = binding.name.clone();
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

/// Build a [`gateway::dns::BindUriProvider`] closure that snapshots the
/// authoritative URI set for `server_name` from [`RootState`] on every
/// invocation. Returns an empty vector if the state is gone or the entry
/// is no longer active.
fn bind_uri_provider(state: Weak<RootState>, server_name: String) -> gateway::dns::BindUriProvider {
    Arc::new(move || {
        let Some(state) = state.upgrade() else {
            return Vec::new();
        };
        // Block on the registry read lock: this runs inside the publish
        // task which is itself spawned on the tokio runtime, so a short
        // synchronous blocking read is acceptable. Use `try_read` to avoid
        // deadlocking against the write lock held during reconcile; a
        // transient miss simply delays the next publish by one tick.
        let Ok(registry) = state.servers.try_read() else {
            tracing::trace!(
                %server_name,
                "bind_uri_provider: registry locked, publishing with empty set"
            );
            return Vec::new();
        };
        match registry.entries.get(&server_name) {
            Some(ServerEntry::Active { bound_ifaces, .. }) => {
                bound_ifaces.keys().cloned().collect()
            }
            _ => Vec::new(),
        }
    })
}
