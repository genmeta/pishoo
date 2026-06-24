use std::{collections::HashMap, sync::Arc};

use dhttp::name::DhttpName;
use gateway::control_plane::{ControlPlane, ProvideListener};
use snafu::Report;

use super::{
    accept::AcceptState,
    snapshot::ServerService,
    source::{ListenerSpec, PrepareContext, PreparedServerUpdate, ServerFingerprint, ServerSource},
};

pub struct ServerRuntime<P>
where
    P: ProvideListener,
{
    name: DhttpName<'static>,
    source: ServerSource,
    listener_spec: ListenerSpec,
    service: Arc<ServerService>,
    accept: AcceptState<P::Listener>,
    fingerprint: ServerFingerprint,
    plane: Arc<P>,
}

pub enum ReloadServerOutcome {
    KeptOldOnPrepareFailure,
    SkippedAcceptNotRunning,
    SwappedServiceOnReusedListener,
    RebuiltListener,
    StoppedAfterFatalRebuild,
}

impl<P> ServerRuntime<P>
where
    P: ProvideListener + Send + Sync + 'static,
    P::Listener: dhttp::h3x::quic::Listen + Send + 'static,
    <P::Listener as dhttp::h3x::quic::Listen>::Error: std::error::Error + Send + Sync + 'static,
    <P::Listener as dhttp::h3x::quic::Listen>::Connection: Send + 'static,
    <<P::Listener as dhttp::h3x::quic::Listen>::Connection as dhttp::h3x::quic::WithLocalAuthority>::LocalAuthority:
        Send + Sync,
    <<P::Listener as dhttp::h3x::quic::Listen>::Connection as dhttp::h3x::quic::WithRemoteAuthority>::RemoteAuthority:
        Send + Sync,
{
    pub fn start(
        source: ServerSource,
        prepared: PreparedServerUpdate,
        plane: Arc<P>,
        listener: P::Listener,
    ) -> Self {
        Self {
            name: prepared.name,
            source,
            listener_spec: prepared.listener_spec,
            service: prepared.service.clone(),
            accept: AcceptState::start(listener, prepared.service),
            fingerprint: prepared.fingerprint,
            plane,
        }
    }

    pub fn name(&self) -> &DhttpName<'static> {
        &self.name
    }

    pub fn fingerprint(&self) -> &ServerFingerprint {
        &self.fingerprint
    }

    pub async fn reload(
        &mut self,
        new_source: ServerSource,
        ctx: &PrepareContext,
    ) -> ReloadServerOutcome {
        let prepared = match new_source.prepare(ctx).await {
            Ok(prepared) => prepared,
            Err(error) => {
                tracing::warn!(
                    server_name = %self.name,
                    error = %Report::from_error(&error),
                    "failed to prepare server reload"
                );
                return ReloadServerOutcome::KeptOldOnPrepareFailure;
            }
        };

        self.apply_reload(new_source, prepared).await
    }

    #[cfg(test)]
    pub(crate) async fn reload_with_prepared(
        &mut self,
        new_source: ServerSource,
        prepared_result: Result<
            PreparedServerUpdate,
            crate::service::source::PrepareServerUpdateError,
        >,
    ) -> ReloadServerOutcome {
        let prepared = match prepared_result {
            Ok(p) => p,
            Err(error) => {
                tracing::warn!(
                    server_name = %self.name,
                    error = %Report::from_error(&error),
                    "failed to prepare server reload"
                );
                return ReloadServerOutcome::KeptOldOnPrepareFailure;
            }
        };
        self.apply_reload(new_source, prepared).await
    }

    async fn apply_reload(
        &mut self,
        new_source: ServerSource,
        prepared: PreparedServerUpdate,
    ) -> ReloadServerOutcome {
        // Future refinement: split hard listener identity from soft updateable fields.
        // The first implementation rebuilds on any ListenRequest fingerprint change,
        // but TLS material or resolver metadata may later become updateable without
        // rebuilding the underlying listener.
        if prepared.listener_spec == self.listener_spec {
            let Some(listener) = self.stop_accept().await else {
                tracing::error!(
                    server_name = %self.name,
                    "accept loop was not running during reload; skipping"
                );
                return ReloadServerOutcome::SkippedAcceptNotRunning;
            };

            self.commit(new_source, prepared);
            self.start_accept(listener);

            ReloadServerOutcome::SwappedServiceOnReusedListener
        } else {
            let old_listener = match self.stop_accept().await {
                Some(listener) => listener,
                None => {
                    tracing::error!(
                        server_name = %self.name,
                        "failed to recover listener before rebuild (was not running)"
                    );
                    return ReloadServerOutcome::StoppedAfterFatalRebuild;
                }
            };

            let request = prepared.listen_request.clone();

            match self.plane.rebuild_listener(old_listener, request).await {
                Ok(new_listener) => {
                    self.commit(new_source, prepared);
                    self.start_accept(new_listener);
                    ReloadServerOutcome::RebuiltListener
                }
                Err(error) => {
                    tracing::error!(
                        server_name = %self.name,
                        error = %Report::from_error(&error),
                        "listener rebuild failed; old listener was consumed by the control plane"
                    );
                    ReloadServerOutcome::StoppedAfterFatalRebuild
                }
            }
        }
    }

    pub async fn remove(mut self) {
        let recovered = self.stop_accept().await;
        if let Some(listener) = recovered {
            let _ = dhttp::h3x::quic::Listen::shutdown(&listener)
                .await
                .inspect_err(|error| {
                    tracing::error!(
                        server_name = %self.name,
                        error = %Report::from_error(error),
                        "failed to shut down listener during removal"
                    );
                });
        }
    }

    async fn stop_accept(&mut self) -> Option<P::Listener> {
        self.accept.take_listener().await.ok()
    }

    fn start_accept(&mut self, listener: P::Listener) {
        self.accept = AcceptState::start(listener, self.service.clone());
    }

    fn commit(&mut self, new_source: ServerSource, prepared: PreparedServerUpdate) {
        self.source = new_source;
        self.listener_spec = prepared.listener_spec;
        self.fingerprint = prepared.fingerprint;
        self.service = prepared.service;
    }

    #[cfg(test)]
    pub(crate) fn accept_state(&self) -> &AcceptState<P::Listener> {
        &self.accept
    }
}

pub struct RuntimeRegistry<P>
where
    P: ProvideListener,
{
    plane: Arc<P>,
    servers: HashMap<DhttpName<'static>, ServerRuntime<P>>,
}

impl<P> RuntimeRegistry<P>
where
    P: ProvideListener + Send + Sync + 'static,
    P::Listener: dhttp::h3x::quic::Listen + Send + 'static,
    <P::Listener as dhttp::h3x::quic::Listen>::Error: std::error::Error + Send + Sync + 'static,
    <P::Listener as dhttp::h3x::quic::Listen>::Connection: Send + 'static,
    <<P::Listener as dhttp::h3x::quic::Listen>::Connection as dhttp::h3x::quic::WithLocalAuthority>::LocalAuthority:
        Send + Sync,
    <<P::Listener as dhttp::h3x::quic::Listen>::Connection as dhttp::h3x::quic::WithRemoteAuthority>::RemoteAuthority:
        Send + Sync,
{
    pub fn new(plane: Arc<P>) -> Self {
        Self {
            plane,
            servers: HashMap::new(),
        }
    }

    pub async fn apply_sources(&mut self, sources: Vec<ServerSource>, ctx: &PrepareContext) {
        let mut new_names = std::collections::HashSet::new();

        for source in sources {
            let name = source.name().clone();
            new_names.insert(name.clone());

            let is_fatal = match self.servers.entry(name.clone()) {
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    let outcome = entry.get_mut().reload(source, ctx).await;
                    matches!(outcome, ReloadServerOutcome::StoppedAfterFatalRebuild)
                }
                std::collections::hash_map::Entry::Vacant(entry) => {
                    let prepared = match source.prepare(ctx).await {
                        Ok(prepared) => prepared,
                        Err(error) => {
                            tracing::warn!(
                                server_name = %name,
                                error = %snafu::Report::from_error(&error),
                                "failed to prepare new server"
                            );
                            continue;
                        }
                    };

                    let listener = match self.plane.listener(prepared.listen_request.clone()).await
                    {
                        Ok(listener) => listener,
                        Err(error) => {
                            tracing::error!(
                                server_name = %name,
                                error = %snafu::Report::from_error(&error),
                                "control plane failed to provide listener"
                            );
                            continue;
                        }
                    };

                    let server =
                        ServerRuntime::start(source, prepared, self.plane.clone(), listener);
                    entry.insert(server);
                    false
                }
            };

            if is_fatal {
                let removed = self.servers.remove(&name);
                if let Some(server) = removed {
                    server.remove().await;
                }
            }
        }

        let mut to_remove = Vec::new();
        for name in self.servers.keys() {
            if !new_names.contains(name) {
                to_remove.push(name.clone());
            }
        }
        for name in to_remove {
            if let Some(server) = self.servers.remove(&name) {
                server.remove().await;
            }
        }
    }

    pub async fn shutdown(mut self) {
        let servers = std::mem::take(&mut self.servers);
        for (_, server) in servers {
            server.remove().await;
        }
    }
}

pub struct WorkerRuntime<P>
where
    P: ControlPlane + ProvideListener,
{
    registry: RuntimeRegistry<P>,
    dhttp_config: dhttp::home::DhttpHome,
    router_state: gateway::reverse::router::RouterState,
}

impl<P> WorkerRuntime<P>
where
    P: ControlPlane + ProvideListener + Send + Sync + 'static,
    P::Listener: dhttp::h3x::quic::Listen + Send + 'static,
    <P::Listener as dhttp::h3x::quic::Listen>::Error: std::error::Error + Send + Sync + 'static,
    <P::Listener as dhttp::h3x::quic::Listen>::Connection: Send + 'static,
    <<P::Listener as dhttp::h3x::quic::Listen>::Connection as dhttp::h3x::quic::WithLocalAuthority>::LocalAuthority:
        Send + Sync,
    <<P::Listener as dhttp::h3x::quic::Listen>::Connection as dhttp::h3x::quic::WithRemoteAuthority>::RemoteAuthority:
        Send + Sync,
{
    pub fn new(
        plane: Arc<P>,
        dhttp_config: dhttp::home::DhttpHome,
        router_state: gateway::reverse::router::RouterState,
    ) -> Self {
        Self {
            registry: RuntimeRegistry::new(plane),
            dhttp_config,
            router_state,
        }
    }

    pub async fn start(&mut self) {
        self.reload().await;
    }

    pub async fn reload(&mut self) {
        let sources =
            match crate::worker::config::load_identity_service_sources(&self.dhttp_config).await {
                Ok(sources) => sources,
                Err(error) => {
                    tracing::warn!(
                        error = %snafu::Report::from_error(&error),
                        "failed to load identity service sources"
                    );
                    return;
                }
            };

        let ctx = match crate::service::source::PrepareContext::load_worker(
            &self.dhttp_config,
            self.router_state.clone(),
        )
        .await
        {
            Ok(ctx) => ctx,
            Err(error) => {
                tracing::warn!(
                    error = %snafu::Report::from_error(&error),
                    "failed to load worker prepare context"
                );
                return;
            }
        };

        let server_sources: Vec<ServerSource> =
            sources.into_iter().map(ServerSource::IdentityService).collect();
        self.registry.apply_sources(server_sources, &ctx).await;
    }

    pub async fn shutdown(self) {
        self.registry.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use snafu::Snafu;

    use super::*;
    use crate::service::source::{ListenerSpec, ServerSource};

    struct FakeListener;

    #[derive(Debug, Snafu)]
    #[snafu(display("fake listener never errors in these tests"))]
    struct FakeListenerError;

    impl dhttp::h3x::quic::Listen for FakeListener {
        type Connection = dhttp::h3x::dquic::prelude::Connection;
        type Error = FakeListenerError;

        async fn accept(&mut self) -> Result<Arc<Self::Connection>, Self::Error> {
            std::future::pending().await
        }

        async fn shutdown(&self) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[derive(Debug, Snafu)]
    #[snafu(display("fake plane never rebuilds in these tests"))]
    struct FakeRebuildError;

    struct FakePlane;

    impl gateway::control_plane::ProvideListener for FakePlane {
        type Listener = FakeListener;
        type ListenError = FakeRebuildError;
        type RebuildError = FakeRebuildError;

        async fn listener(
            &self,
            _request: gateway::control_plane::ListenRequest,
        ) -> Result<Self::Listener, Self::ListenError> {
            Err(FakeRebuildError)
        }

        async fn rebuild_listener(
            &self,
            _old: Self::Listener,
            _request: gateway::control_plane::ListenRequest,
        ) -> Result<Self::Listener, Self::RebuildError> {
            Err(FakeRebuildError)
        }
    }

    fn fake_runtime(name: &str, generation: u64) -> ServerRuntime<FakePlane> {
        let spec = ListenerSpec::fake("same");
        let source = ServerSource::fake_success(name, generation, spec.clone());
        let name_owned = DhttpName::try_from(name.to_owned()).expect("valid dhttp name");
        ServerRuntime {
            name: name_owned,
            source,
            listener_spec: spec.clone(),
            service: Arc::new(ServerService::fake()),
            accept: AcceptState::Stopped {
                listener: FakeListener,
            },
            fingerprint: ServerFingerprint {
                listener_spec: spec,
                service_generation: generation,
            },
            plane: Arc::new(FakePlane),
        }
    }

    #[tokio::test]
    async fn reload_prepare_failure_keeps_current_fingerprint() {
        let mut runtime = fake_runtime("alpha.example", 1);
        let failing_source = ServerSource::fake_prepare_error("alpha.example");
        let prepared_result = if let ServerSource::Fake(fake) = &failing_source {
            fake.prepare()
        } else {
            unreachable!()
        };

        let outcome = runtime
            .reload_with_prepared(failing_source, prepared_result)
            .await;

        assert!(matches!(
            outcome,
            ReloadServerOutcome::KeptOldOnPrepareFailure
        ));
        assert_eq!(runtime.fingerprint().generation_for_test(), 1);
    }

    #[tokio::test]
    async fn unchanged_listener_reload_swaps_service_for_future_accepts() {
        let mut runtime = fake_runtime("alpha.example", 1);
        let source = ServerSource::fake_success("alpha.example", 2, ListenerSpec::fake("same"));
        let prepared_result = if let ServerSource::Fake(fake) = &source {
            fake.prepare()
        } else {
            unreachable!()
        };

        let outcome = runtime.reload_with_prepared(source, prepared_result).await;

        assert!(matches!(
            outcome,
            ReloadServerOutcome::SwappedServiceOnReusedListener
        ));
        assert_eq!(runtime.fingerprint().generation_for_test(), 2);
    }

    #[tokio::test]
    async fn reused_listener_reload_keeps_accept_running() {
        let mut runtime = fake_runtime("alpha.example", 1);
        let source = ServerSource::fake_success("alpha.example", 2, ListenerSpec::fake("same"));
        let prepared_result = if let ServerSource::Fake(fake) = &source {
            fake.prepare()
        } else {
            unreachable!()
        };

        let _ = runtime.reload_with_prepared(source, prepared_result).await;

        assert!(matches!(
            runtime.accept_state(),
            AcceptState::Running { .. }
        ));
    }

    #[tokio::test]
    async fn remove_completes_even_when_release_fails() {
        struct FailingListener {}
        impl dhttp::h3x::quic::Listen for FailingListener {
            type Connection = dhttp::h3x::dquic::prelude::Connection;
            type Error = FakeListenerError;
            async fn accept(&mut self) -> Result<Arc<Self::Connection>, Self::Error> {
                std::future::pending().await
            }
            async fn shutdown(&self) -> Result<(), Self::Error> {
                Err(FakeListenerError)
            }
        }

        struct FailingPlane;

        impl gateway::control_plane::ProvideListener for FailingPlane {
            type Listener = FailingListener;
            type ListenError = FakeRebuildError;
            type RebuildError = FakeRebuildError;

            async fn listener(
                &self,
                _request: gateway::control_plane::ListenRequest,
            ) -> Result<Self::Listener, Self::ListenError> {
                Err(FakeRebuildError)
            }
            async fn rebuild_listener(
                &self,
                _old: Self::Listener,
                _request: gateway::control_plane::ListenRequest,
            ) -> Result<Self::Listener, Self::RebuildError> {
                Err(FakeRebuildError)
            }
        }

        let runtime = ServerRuntime {
            name: DhttpName::try_from("alpha.example".to_owned()).unwrap(),
            source: ServerSource::fake_success("alpha.example", 1, ListenerSpec::fake("same")),
            listener_spec: ListenerSpec::fake("same"),
            service: Arc::new(ServerService::fake()),
            accept: AcceptState::Stopped {
                listener: FailingListener {},
            },
            fingerprint: ServerFingerprint {
                listener_spec: ListenerSpec::fake("same"),
                service_generation: 1,
            },
            plane: Arc::new(FailingPlane),
        };

        runtime.remove().await; // Should not panic or hang
    }
}
