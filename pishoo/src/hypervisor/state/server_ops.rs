//! Listener registry operations on [`RootState`].

use std::sync::Arc;

use dhttp::{h3x::quic::Listen as _, name::DhttpName};
use gateway::control_plane::ListenRequest;
use snafu::{IntoError, ResultExt};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use super::{
    AcquireListenerError, ListenerResource, RebuildListenerError, ReleaseListenerError, RootState,
    acquire_listener_error,
    listener_registry::{AcquirePlan, RebuildPlan, ReleasePlan},
    owner::Owner,
    rebuild_listener_error,
};
use crate::{
    hypervisor::{endpoint_factory, resource::AsyncReleaseGuard},
    listen::RegisteredEndpoint,
};

fn endpoint_publication_loop(
    endpoint: &dhttp::endpoint::Endpoint,
) -> Result<
    dhttp::ddns::publishers::EndpointPublicationLoop<
        dhttp::ddns::publishers::EndpointBindingAddresses,
    >,
    dhttp::endpoint::CreateEndpointPublicationLoopError,
> {
    endpoint.dns_publication_loop()
}

struct BuiltListener {
    resource: ListenerResource,
    registered: RegisteredEndpoint,
    guard: AsyncReleaseGuard,
}

type AcquireListenerSender = oneshot::Sender<Result<RegisteredEndpoint, AcquireListenerError>>;
type RebuildListenerSender = oneshot::Sender<Result<RegisteredEndpoint, RebuildListenerError>>;

struct AcquireListenerTransition {
    owner: Owner,
    server_name: DhttpName<'static>,
    request: ListenRequest,
    bind_patterns: Arc<Vec<dhttp::h3x::dquic::binds::BindPattern>>,
    done: crate::hypervisor::resource::Completion,
    tx: AcquireListenerSender,
}

struct RebuildListenerTransition {
    owner: Owner,
    server_name: DhttpName<'static>,
    request: ListenRequest,
    bind_patterns: Arc<Vec<dhttp::h3x::dquic::binds::BindPattern>>,
    old_resource: ListenerResource,
    done: crate::hypervisor::resource::Completion,
    tx: RebuildListenerSender,
}

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
                    let (tx, rx) = oneshot::channel();
                    self.spawn_acquire_listener_transition(AcquireListenerTransition {
                        owner,
                        server_name,
                        request,
                        bind_patterns,
                        done,
                        tx,
                    });
                    return rx
                        .await
                        .unwrap_or(Err(AcquireListenerError::TransitionStopped));
                }
                AcquirePlan::Wait(done) => done.wait().await,
                AcquirePlan::Duplicate => return Err(AcquireListenerError::DuplicateListen),
                AcquirePlan::Conflict => return Err(AcquireListenerError::ConflictedName),
                AcquirePlan::DestroyConflict {
                    owner: existing_owner,
                    resource,
                    guard,
                    done,
                } => {
                    self.spawn_conflict_listener_transition(
                        existing_owner,
                        server_name.clone(),
                        resource,
                        guard,
                        done.clone(),
                    );
                    done.wait().await;
                    return Err(AcquireListenerError::ConflictedName);
                }
            }
        }
    }

    /// Release a listener owned by the caller.
    pub async fn release_listener(
        self: &Arc<Self>,
        owner: Owner,
        server_name: &DhttpName<'static>,
    ) -> Result<(), ReleaseListenerError> {
        self.release_listener_inner(owner, server_name, None).await
    }

    pub(crate) async fn release_listener_for_handle(
        self: &Arc<Self>,
        owner: Owner,
        server_name: &DhttpName<'static>,
        guard: AsyncReleaseGuard,
    ) -> Result<(), ReleaseListenerError> {
        self.release_listener_inner(owner, server_name, Some(guard))
            .await
    }

    pub(crate) fn release_listener_for_dropped_handle(
        self: &Arc<Self>,
        owner: Owner,
        server_name: DhttpName<'static>,
        guard: AsyncReleaseGuard,
    ) {
        let state = self.clone();
        self.spawn_resource_transition(
            async move {
                if let Err(error) = state
                    .release_listener_for_handle(owner, &server_name, guard)
                    .await
                {
                    tracing::warn!(
                        %server_name,
                        error = %snafu::Report::from_error(&error),
                        "failed to release dropped listener handle"
                    );
                }
            }
            .in_current_span(),
        );
    }

    async fn release_listener_inner(
        self: &Arc<Self>,
        owner: Owner,
        server_name: &DhttpName<'static>,
        guard: Option<AsyncReleaseGuard>,
    ) -> Result<(), ReleaseListenerError> {
        loop {
            let plan = {
                let mut registry = self.listeners.write().await;
                registry.plan_release(owner, server_name, guard.as_ref())
            };

            match plan {
                ReleasePlan::Destroy {
                    resource,
                    guard: active_guard,
                    done,
                } => {
                    active_guard.disarm();
                    self.spawn_release_listener_transition(
                        owner,
                        server_name.clone(),
                        resource,
                        done.clone(),
                    );
                    done.wait().await;
                    return Ok(());
                }
                ReleasePlan::Wait(done) => done.wait().await,
                ReleasePlan::NotOwner => return Err(ReleaseListenerError::NotOwner),
                ReleasePlan::NotFound | ReleasePlan::StaleHandle | ReleasePlan::Poisoned => {
                    return Ok(());
                }
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
        let bind_patterns = request
            .bind
            .iter()
            .map(gateway::parse::types::Listens::try_to_bind_patterns)
            .collect::<Result<Vec<_>, _>>()
            .context(acquire_listener_error::BuildBindPatternsSnafu)
            .context(rebuild_listener_error::ReplacementSnafu)?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        let bind_patterns = Arc::new(bind_patterns);

        loop {
            let plan = {
                let mut registry = self.listeners.write().await;
                registry.plan_rebuild(owner, &server_name)
            };

            match plan {
                RebuildPlan::Rebuild {
                    resource,
                    guard,
                    done,
                } => {
                    guard.disarm();
                    let (tx, rx) = oneshot::channel();
                    self.spawn_rebuild_listener_transition(RebuildListenerTransition {
                        owner,
                        server_name,
                        request,
                        bind_patterns,
                        old_resource: resource,
                        done,
                        tx,
                    });
                    return rx
                        .await
                        .unwrap_or(Err(RebuildListenerError::TransitionStopped));
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

    fn spawn_acquire_listener_transition(self: &Arc<Self>, transition: AcquireListenerTransition) {
        let state = self.clone();
        self.spawn_resource_transition(
            async move {
                state.run_acquire_listener_transition(transition).await;
            }
            .in_current_span(),
        );
    }

    async fn run_acquire_listener_transition(
        self: Arc<Self>,
        transition: AcquireListenerTransition,
    ) {
        if transition.tx.is_closed() {
            self.finish_listener_transition_vacant(
                transition.owner,
                &transition.server_name,
                &transition.done,
            )
            .await;
            return;
        }

        let built = self
            .build_listener_resource(
                transition.owner,
                &transition.server_name,
                &transition.request,
                transition.bind_patterns,
            )
            .await;
        match built {
            Ok(built) => {
                self.pause_listener_delivery_for_test().await;
                self.commit_and_deliver_acquired_listener(
                    transition.owner,
                    transition.server_name,
                    transition.done,
                    built,
                    transition.tx,
                )
                .await;
            }
            Err(error) => {
                self.finish_listener_transition_vacant(
                    transition.owner,
                    &transition.server_name,
                    &transition.done,
                )
                .await;
                let _ = transition.tx.send(Err(error));
            }
        }
    }

    fn spawn_conflict_listener_transition(
        self: &Arc<Self>,
        owner: Owner,
        server_name: DhttpName<'static>,
        resource: ListenerResource,
        guard: AsyncReleaseGuard,
        done: crate::hypervisor::resource::Completion,
    ) {
        guard.disarm();
        let state = self.clone();
        self.spawn_resource_transition(
            async move {
                state.destroy_listener_resource(resource).await;
                let mut registry = state.listeners.write().await;
                registry.finish_transition_poisoned(owner, &server_name, &done);
            }
            .in_current_span(),
        );
    }

    fn spawn_release_listener_transition(
        self: &Arc<Self>,
        owner: Owner,
        server_name: DhttpName<'static>,
        resource: ListenerResource,
        done: crate::hypervisor::resource::Completion,
    ) {
        let state = self.clone();
        self.spawn_resource_transition(
            async move {
                state.destroy_listener_resource(resource).await;
                state
                    .finish_listener_transition_vacant(owner, &server_name, &done)
                    .await;
            }
            .in_current_span(),
        );
    }

    fn spawn_rebuild_listener_transition(self: &Arc<Self>, transition: RebuildListenerTransition) {
        let state = self.clone();
        self.spawn_resource_transition(
            async move {
                state.run_rebuild_listener_transition(transition).await;
            }
            .in_current_span(),
        );
    }

    async fn run_rebuild_listener_transition(
        self: Arc<Self>,
        transition: RebuildListenerTransition,
    ) {
        self.destroy_listener_resource(transition.old_resource)
            .await;

        let built = self
            .build_listener_resource(
                transition.owner,
                &transition.server_name,
                &transition.request,
                transition.bind_patterns,
            )
            .await;
        match built {
            Ok(built) => {
                self.pause_listener_delivery_for_test().await;
                self.commit_and_deliver_rebuilt_listener(
                    transition.owner,
                    transition.server_name,
                    transition.done,
                    built,
                    transition.tx,
                )
                .await;
            }
            Err(error) => {
                self.finish_listener_transition_vacant(
                    transition.owner,
                    &transition.server_name,
                    &transition.done,
                )
                .await;
                let _ = transition
                    .tx
                    .send(Err(RebuildListenerError::Replacement { source: error }));
            }
        }
    }

    async fn commit_and_deliver_acquired_listener(
        &self,
        owner: Owner,
        server_name: DhttpName<'static>,
        done: crate::hypervisor::resource::Completion,
        built: BuiltListener,
        tx: oneshot::Sender<Result<RegisteredEndpoint, AcquireListenerError>>,
    ) {
        let BuiltListener {
            resource,
            registered,
            guard,
        } = built;
        match self
            .commit_listener_transition_active(
                owner,
                server_name.clone(),
                &done,
                resource,
                guard.clone(),
            )
            .await
        {
            Ok(()) => {
                self.deliver_acquired_listener(server_name, registered, tx)
                    .await;
            }
            Err(resource) => {
                done.complete();
                self.destroy_uncommitted_listener(resource, registered, guard)
                    .await;
                let _ = tx.send(Err(AcquireListenerError::ConflictedName));
            }
        }
    }

    async fn commit_and_deliver_rebuilt_listener(
        &self,
        owner: Owner,
        server_name: DhttpName<'static>,
        done: crate::hypervisor::resource::Completion,
        built: BuiltListener,
        tx: oneshot::Sender<Result<RegisteredEndpoint, RebuildListenerError>>,
    ) {
        let BuiltListener {
            resource,
            registered,
            guard,
        } = built;
        match self
            .commit_listener_transition_active(
                owner,
                server_name.clone(),
                &done,
                resource,
                guard.clone(),
            )
            .await
        {
            Ok(()) => {
                self.deliver_rebuilt_listener(server_name, registered, tx)
                    .await;
            }
            Err(resource) => {
                done.complete();
                self.destroy_uncommitted_listener(resource, registered, guard)
                    .await;
                let _ = tx.send(Err(RebuildListenerError::ConflictedName));
            }
        }
    }

    async fn commit_listener_transition_active(
        &self,
        owner: Owner,
        server_name: DhttpName<'static>,
        done: &crate::hypervisor::resource::Completion,
        resource: ListenerResource,
        guard: AsyncReleaseGuard,
    ) -> Result<(), ListenerResource> {
        let mut registry = self.listeners.write().await;
        registry.commit_transition_active(owner, server_name, done, resource, guard)
    }

    async fn deliver_acquired_listener(
        &self,
        server_name: DhttpName<'static>,
        registered: RegisteredEndpoint,
        tx: oneshot::Sender<Result<RegisteredEndpoint, AcquireListenerError>>,
    ) {
        match tx.send(Ok(registered)) {
            Ok(()) => {}
            Err(Ok(registered)) => {
                self.shutdown_undelivered_listener(server_name, registered)
                    .await;
            }
            Err(Err(_)) => {}
        }
    }

    async fn deliver_rebuilt_listener(
        &self,
        server_name: DhttpName<'static>,
        registered: RegisteredEndpoint,
        tx: oneshot::Sender<Result<RegisteredEndpoint, RebuildListenerError>>,
    ) {
        match tx.send(Ok(registered)) {
            Ok(()) => {}
            Err(Ok(registered)) => {
                self.shutdown_undelivered_listener(server_name, registered)
                    .await;
            }
            Err(Err(_)) => {}
        }
    }

    async fn shutdown_undelivered_listener(
        &self,
        server_name: DhttpName<'static>,
        registered: RegisteredEndpoint,
    ) {
        if let Err(error) = dhttp::h3x::quic::Listen::shutdown(&registered).await {
            tracing::warn!(
                %server_name,
                error = %snafu::Report::from_error(&error),
                "failed to release undelivered listener"
            );
        }
    }

    async fn destroy_uncommitted_listener(
        &self,
        resource: ListenerResource,
        registered: RegisteredEndpoint,
        guard: AsyncReleaseGuard,
    ) {
        guard.disarm();
        drop(registered);
        resource.destroy().await;
    }

    async fn destroy_listener_resource(&self, resource: ListenerResource) {
        self.pause_listener_destroy_for_test().await;
        resource.destroy().await;
    }

    async fn finish_listener_transition_vacant(
        &self,
        owner: Owner,
        server_name: &DhttpName<'static>,
        done: &crate::hypervisor::resource::Completion,
    ) {
        let mut registry = self.listeners.write().await;
        registry.finish_transition_vacant(owner, server_name, done);
    }

    async fn build_listener_resource(
        self: &Arc<Self>,
        owner: Owner,
        server_name: &DhttpName<'static>,
        request: &ListenRequest,
        bind_patterns: Arc<Vec<dhttp::h3x::dquic::binds::BindPattern>>,
    ) -> Result<BuiltListener, AcquireListenerError> {
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
        let publisher_loop = match endpoint_publication_loop(&endpoint) {
            Ok(publisher_loop) => publisher_loop,
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
        let publish_task = Some(release_scope.spawn_handle(move |owner_token| {
            async move {
                tokio::select! {
                    biased;
                    () = owner_token.cancelled() => {}
                    () = publish_shutdown.cancelled() => {}
                    () = async { publisher_loop.run().await } => {}
                }
            }
            .in_current_span()
        }));
        let guard = AsyncReleaseGuard::new();
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
            guard.clone(),
        );
        Ok(BuiltListener {
            resource,
            registered,
            guard,
        })
    }

    pub(crate) async fn task_scope_for_owner(
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

    #[cfg(test)]
    pub(crate) fn pause_next_listener_destroy_for_test(&self) -> super::ListenerPause {
        self.listener_test_hooks.set_next_destroy()
    }

    #[cfg(test)]
    pub(crate) fn pause_next_listener_delivery_for_test(&self) -> super::ListenerPause {
        self.listener_test_hooks.set_next_delivery()
    }

    #[cfg(test)]
    async fn pause_listener_destroy_for_test(&self) {
        if let Some(pause) = self.listener_test_hooks.take_next_destroy() {
            pause.pause().await;
        }
    }

    #[cfg(not(test))]
    async fn pause_listener_destroy_for_test(&self) {}

    #[cfg(test)]
    async fn pause_listener_delivery_for_test(&self) {
        if let Some(pause) = self.listener_test_hooks.take_next_delivery() {
            pause.pause().await;
        }
    }

    #[cfg(not(test))]
    async fn pause_listener_delivery_for_test(&self) {}
}
