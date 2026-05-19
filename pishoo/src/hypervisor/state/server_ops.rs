//! Server registry operations on [`RootState`].

use std::sync::Arc;

use dhttp::{
    ddns::{DHTTP_H3_DNS_SERVER, Resolvers},
    endpoint::Endpoint,
    identity::Identity,
    name::DhttpName,
};
use gateway::control_plane::ListenRequest;
use h3x::{dquic::binds::BindPattern, quic::Listen as _};
use http::Uri;
use snafu::IntoError;
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};
use tracing::Instrument;

use super::{RegisterError, RetiredServer, RootState, ServerEntry, ServiceOwner, register_error};
use crate::listen::RegisteredEndpoint;

impl RootState {
    // -----------------------------------------------------------------------
    // Server registry
    // -----------------------------------------------------------------------

    /// Unified entry point for registering a listener.
    ///
    /// State machine:
    /// - **Vacant** → insert `Registering` sentinel → build DHTTP endpoint
    ///   → promote to `Active`.
    /// - **Registering/Active, same owner** → `DuplicateListen`.
    /// - **Registering/Active, different owner** → poison to `Conflicted`.
    /// - **Conflicted** → `ConflictedName`.
    ///
    /// Returns a [`RegisteredEndpoint`] on success.
    pub async fn register_listener(
        self: &Arc<Self>,
        owner: ServiceOwner,
        request: ListenRequest,
    ) -> Result<RegisteredEndpoint, RegisterError> {
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
                    drop(registry);

                    if let Some(ServerEntry::Active {
                        owner: old_owner,
                        endpoint,
                        shutdown_token,
                        publish_task,
                    }) = old
                    {
                        if let ServiceOwner::Worker(pid) = old_owner {
                            let mut inner = self.inner.lock().await;
                            if let Some(proc) = inner.processes.get_mut(&pid) {
                                proc.owned_servers.remove(&server_name);
                            }
                        }
                        RetiredServer {
                            endpoint,
                            shutdown_token,
                            publish_task,
                        }
                        .shutdown()
                        .await;
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
        // BindPatterns and let dhttp::Endpoint bind them on the shared Network.
        // No lock held — other server_names can be read/written concurrently.
        let bind_patterns = request
            .bind
            .iter()
            .flat_map(gateway::parse::Listens::to_bind_patterns)
            .collect::<Vec<_>>();
        let identity = Arc::new(request.identity.clone());
        let bind_patterns = Arc::new(bind_patterns);
        let resolver = match self
            .build_endpoint_resolver(
                identity.clone(),
                bind_patterns.clone(),
                request.dns_resolver_url.clone(),
            )
            .await
        {
            Ok(resolver) => resolver,
            Err(source) => {
                self.servers.write().await.entries.remove(&server_name);
                return Err(register_error::BuildResolverSnafu.into_error(source));
            }
        };
        let endpoint = Endpoint::builder()
            .network(self.network.clone())
            .identity(identity)
            .server(self.server_qcfg.clone())
            .bind(bind_patterns)
            .resolver(Arc::new(resolver))
            .build()
            .await;
        let shutdown_token = CancellationToken::new();

        let publisher = match endpoint.publisher_with_options(request.publish_options) {
            Ok(publisher) => publisher,
            Err(source) => {
                self.servers.write().await.entries.remove(&server_name);
                if let Err(error) = endpoint.shutdown().await {
                    tracing::warn!(
                        %server_name,
                        error = %snafu::Report::from_error(&error),
                        "failed to shut down endpoint after publisher setup failed"
                    );
                }
                return Err(register_error::CreatePublisherSnafu.into_error(source));
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
                    // Sentinel was replaced (e.g. by cross-owner conflict or
                    // cleanup). Shut down the endpoint so its binds/SNI do not
                    // leak.
                    drop(registry);
                    if let Err(error) = endpoint.shutdown().await {
                        tracing::warn!(
                            %server_name,
                            error = %snafu::Report::from_error(&error),
                            "failed to shut down rolled back endpoint"
                        );
                    }
                    tracing::warn!(
                        %server_name,
                        ?owner,
                        "sentinel lost during register_listener; rolled back"
                    );
                    return Err(RegisterError::ConflictedName);
                }
            }

            let publish_task = Some(AbortOnDropHandle::new(tokio::spawn(
                async move {
                    publisher.run().await;
                }
                .in_current_span(),
            )));

            registry.entries.insert(
                server_name.clone(),
                ServerEntry::Active {
                    owner,
                    endpoint: endpoint.clone(),
                    shutdown_token: shutdown_token.clone(),
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
                    if let Some(retired) = retired {
                        retired.shutdown().await;
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
            Ok(RegisteredEndpoint::new_registered(
                endpoint,
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

        if let Some(retired) = retired {
            retired.shutdown().await;
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

    async fn build_endpoint_resolver(
        self: &Arc<Self>,
        identity: Arc<Identity>,
        bind_patterns: Arc<Vec<BindPattern>>,
        dns_resolver_url: Option<Uri>,
    ) -> std::io::Result<Resolvers> {
        let dns_endpoint = Endpoint::builder()
            .network(self.network.clone())
            .identity(identity)
            .server(self.server_qcfg.clone())
            .bind(bind_patterns.clone())
            .resolver(Arc::new(dhttp::dquic::resolver::handy::SystemResolver))
            .build()
            .await;

        let base_url = dns_resolver_url
            .map(|url| url.to_string())
            .unwrap_or_else(|| DHTTP_H3_DNS_SERVER.to_owned());

        let builder = Resolvers::builder()
            .mdns(self.network.clone(), bind_patterns)
            .await
            .system()
            .h3_with_base_url(base_url, dns_endpoint.as_h3())?;

        Ok(builder.build())
    }
}
