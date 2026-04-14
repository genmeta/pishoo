//! Server registry operations on [`RootState`].

use std::sync::Arc;

use gateway::control_plane::ListenRequest;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use super::{RegisterError, RootState, ServerEntry, ServiceOwner};
use crate::listen::PerServerListener;

impl RootState {
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
        let bind_uris = crate::listen::resolve_bind_uris(&request.bind, &device_names);

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
            let desired_uris: Vec<String> =
                crate::listen::resolve_bind_uris(&listens, &device_names);

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
}
