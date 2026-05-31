//! Listener registry operations on [`RootState`].

use std::sync::Arc;

use dhttp::name::DhttpName;
use gateway::control_plane::ListenRequest;
use h3x::quic::Listen as _;
use snafu::{IntoError, ResultExt};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use super::{
    AcquireListenerError, ListenerResource, RebuildListenerError, ReleaseListenerError, RootState,
    acquire_listener_error,
    listener_registry::{AcquirePlan, DestroyFinish, DestroyReason, RebuildPlan, ReleasePlan},
    owner::Owner,
    rebuild_listener_error,
};
use crate::{hypervisor::endpoint_factory, listen::RegisteredEndpoint};

impl RootState {
    // -----------------------------------------------------------------------
    // Listener registry
    // -----------------------------------------------------------------------

    /// Acquire a listener for an owner and listen request.
    pub async fn acquire_listener(
        self: &Arc<Self>,
        owner: Owner,
        request: ListenRequest,
    ) -> Result<RegisteredEndpoint, AcquireListenerError> {
        let server_name = self.listener_name(&request);
        let bind_patterns = request
            .bind
            .iter()
            .map(gateway::parse::types::Listens::try_to_bind_patterns)
            .collect::<Result<Vec<_>, _>>()
            .context(acquire_listener_error::BuildBindPatternsSnafu)?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        let bind_patterns = Arc::new(bind_patterns);

        loop {
            let plan = {
                let mut registry = self.listeners.write().await;
                registry.plan_acquire(owner, server_name.clone())
            };

            match plan {
                AcquirePlan::Build { done } => {
                    let built = self
                        .build_listener_resource(
                            owner,
                            &server_name,
                            &request,
                            bind_patterns.clone(),
                        )
                        .await;
                    return self
                        .commit_acquired_listener(owner, server_name, done, built)
                        .await;
                }
                AcquirePlan::Wait(done) => done.wait().await,
                AcquirePlan::Duplicate => return Err(AcquireListenerError::DuplicateListen),
                AcquirePlan::Conflict => return Err(AcquireListenerError::ConflictedName),
                AcquirePlan::DestroyConflict {
                    owner: old_owner,
                    resource,
                    done,
                } => {
                    resource.destroy().await;
                    self.remove_owned_listener(old_owner, &server_name).await;
                    let mut registry = self.listeners.write().await;
                    registry.finish_destroying(&server_name, &done, DestroyFinish::Poisoned);
                    return Err(AcquireListenerError::ConflictedName);
                }
            }
        }
    }

    /// Release a listener owned by the caller.
    pub async fn release_listener(
        &self,
        owner: Owner,
        server_name: &DhttpName<'static>,
    ) -> Result<(), ReleaseListenerError> {
        loop {
            let plan = {
                let mut registry = self.listeners.write().await;
                registry.plan_release(owner, server_name, DestroyReason::Release)
            };

            match plan {
                ReleasePlan::Destroy { resource, done } => {
                    resource.destroy().await;
                    self.remove_owned_listener(owner, server_name).await;
                    let mut registry = self.listeners.write().await;
                    registry.finish_destroying(server_name, &done, DestroyFinish::Vacant);
                    return Ok(());
                }
                ReleasePlan::Wait(done) => done.wait().await,
                ReleasePlan::NotOwner => return Err(ReleaseListenerError::NotOwner),
                ReleasePlan::NotFound | ReleasePlan::Poisoned => return Ok(()),
            }
        }
    }

    /// Rebuild an owned listener without exposing a vacant interleaving window.
    pub async fn rebuild_listener(
        self: &Arc<Self>,
        owner: Owner,
        request: ListenRequest,
    ) -> Result<RegisteredEndpoint, RebuildListenerError> {
        let server_name = self.listener_name(&request);

        loop {
            let plan = {
                let mut registry = self.listeners.write().await;
                registry.plan_rebuild(owner, &server_name)
            };

            match plan {
                RebuildPlan::Destroy { resource, done } => {
                    resource.destroy().await;
                    self.remove_owned_listener(owner, &server_name).await;

                    let creating_done = {
                        let mut registry = self.listeners.write().await;
                        registry.begin_creating_after_destroy(owner, server_name.clone(), &done)
                    };
                    let Some(creating_done) = creating_done else {
                        return Err(RebuildListenerError::NotOwner);
                    };

                    let bind_patterns = match request
                        .bind
                        .iter()
                        .map(gateway::parse::types::Listens::try_to_bind_patterns)
                        .collect::<Result<Vec<_>, _>>()
                    {
                        Ok(patterns) => patterns.into_iter().flatten().collect::<Vec<_>>(),
                        Err(source) => {
                            let mut registry = self.listeners.write().await;
                            registry.abort_creating(owner, &server_name, &creating_done);
                            return Err(RebuildListenerError::Replacement {
                                source: AcquireListenerError::BuildBindPatterns { source },
                            });
                        }
                    };
                    let built = self
                        .build_listener_resource(
                            owner,
                            &server_name,
                            &request,
                            Arc::new(bind_patterns),
                        )
                        .await;
                    return self
                        .commit_acquired_listener(owner, server_name, creating_done, built)
                        .await
                        .context(rebuild_listener_error::ReplacementSnafu);
                }
                RebuildPlan::Wait(done) => done.wait().await,
                RebuildPlan::NotOwner => return Err(RebuildListenerError::NotOwner),
                RebuildPlan::NotFound => {
                    return self
                        .acquire_listener(owner, request)
                        .await
                        .context(rebuild_listener_error::ReplacementSnafu);
                }
                RebuildPlan::Conflict => return Err(RebuildListenerError::ConflictedName),
            }
        }
    }

    /// Remove all poisoned listener entries from the registry.
    pub async fn clear_listener_poison(&self) -> Vec<DhttpName<'static>> {
        let mut registry = self.listeners.write().await;
        let cleared = registry.clear_poisoned();
        if !cleared.is_empty() {
            tracing::info!(
                count = cleared.len(),
                names = ?cleared,
                "cleared poisoned listener entries during reload"
            );
        }
        cleared
    }

    pub(crate) async fn owner_for_pid(&self, pid: nix::unistd::Pid) -> Option<Owner> {
        let inner = self.inner.lock().await;
        let process = inner.processes.get(&pid)?;
        Some(Owner::worker(process.uid, pid))
    }

    fn listener_name(&self, request: &ListenRequest) -> DhttpName<'static> {
        DhttpName::try_from(request.identity.name().as_full().to_owned())
            .expect("listen request identity must be a dhttp name")
    }

    async fn build_listener_resource(
        self: &Arc<Self>,
        owner: Owner,
        server_name: &DhttpName<'static>,
        request: &ListenRequest,
        bind_patterns: Arc<Vec<h3x::dquic::binds::BindPattern>>,
    ) -> Result<(ListenerResource, RegisteredEndpoint), AcquireListenerError> {
        let release_scope = self
            .task_scope_for_owner(owner)
            .await
            .ok_or(AcquireListenerError::OwnerUnavailable)?;

        let identity = Arc::new(request.identity.clone());
        let resolver = endpoint_factory::build_resolver(
            identity.clone(),
            self.network.clone(),
            bind_patterns.clone(),
            request.dns_resolver_url.clone(),
        )
        .await
        .context(acquire_listener_error::BuildResolverSnafu)?;
        let endpoint = endpoint_factory::build_registered_endpoint(
            identity,
            self.network.clone(),
            bind_patterns,
            resolver,
        )
        .await
        .context(acquire_listener_error::BuildEndpointSnafu)?;
        let shutdown_token = CancellationToken::new();
        let publisher = match endpoint.publisher_with_options(request.publish_options) {
            Ok(publisher) => publisher,
            Err(source) => {
                if let Err(error) = endpoint.shutdown().await {
                    tracing::warn!(
                        %server_name,
                        error = %snafu::Report::from_error(&error),
                        "failed to shut down endpoint after publisher setup failed"
                    );
                }
                return Err(acquire_listener_error::CreatePublisherSnafu.into_error(source));
            }
        };

        let publish_token = CancellationToken::new();
        let publish_shutdown = publish_token.clone();
        let publish_task = Some(release_scope.spawn(move |owner_token| {
            async move {
                tokio::select! {
                    () = owner_token.cancelled() => {}
                    () = publish_shutdown.cancelled() => {}
                    () = async { publisher.run().await } => {}
                }
            }
            .in_current_span()
        }));
        let resource = ListenerResource::new(
            endpoint.clone(),
            shutdown_token.clone(),
            publish_token,
            publish_task,
        );
        let registered = RegisteredEndpoint::new_registered(
            endpoint,
            shutdown_token,
            self,
            server_name.clone(),
            owner,
        );
        Ok((resource, registered))
    }

    async fn commit_acquired_listener(
        &self,
        owner: Owner,
        server_name: DhttpName<'static>,
        done: super::completion::Completion,
        built: Result<(ListenerResource, RegisteredEndpoint), AcquireListenerError>,
    ) -> Result<RegisteredEndpoint, AcquireListenerError> {
        let (resource, registered) = match built {
            Ok(built) => built,
            Err(error) => {
                let mut registry = self.listeners.write().await;
                registry.abort_creating(owner, &server_name, &done);
                return Err(error);
            }
        };

        let committed = {
            let mut registry = self.listeners.write().await;
            registry.commit_creating(owner, server_name.clone(), &done, resource)
        };
        if committed {
            self.record_owned_listener(owner, server_name).await;
            Ok(registered)
        } else {
            registered.destroy_without_registry_release().await;
            Err(AcquireListenerError::ConflictedName)
        }
    }

    async fn task_scope_for_owner(
        &self,
        owner: Owner,
    ) -> Option<crate::hypervisor::task_scope::TaskScope> {
        match owner {
            Owner::Local => Some(self.local_tasks.clone()),
            Owner::Worker { uid, pid } => {
                let inner = self.inner.lock().await;
                let process = inner.processes.get(&pid)?;
                (process.uid == uid).then(|| process.tasks.clone())
            }
        }
    }

    async fn record_owned_listener(&self, owner: Owner, server_name: DhttpName<'static>) {
        let Owner::Worker { pid, .. } = owner else {
            return;
        };
        let mut inner = self.inner.lock().await;
        if let Some(process) = inner.processes.get_mut(&pid) {
            process.owned_servers.insert(server_name);
        }
    }

    async fn remove_owned_listener(&self, owner: Owner, server_name: &DhttpName<'static>) {
        let Owner::Worker { pid, .. } = owner else {
            return;
        };
        let mut inner = self.inner.lock().await;
        if let Some(process) = inner.processes.get_mut(&pid) {
            process.owned_servers.remove(server_name);
        }
    }
}
