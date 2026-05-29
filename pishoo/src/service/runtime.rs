#![allow(dead_code)]

use std::sync::Arc;

use dhttp::name::DhttpName;
use gateway::control_plane::ProvideListener;
use snafu::Report;

use super::{
    accept::AcceptState,
    snapshot::ServerService,
    source::{
        ListenerSpec, PreparedServerUpdate, ServerFingerprint, ServerSource, WorkerPrepareContext,
        WorkerServerSource,
    },
};

/// Task 7 Phase 2 transitional trait: ProvideListener is supposed to have
/// `release_listener` but it's missing from `gateway/src/control_plane.rs`
/// in this branch. We define it here to satisfy the requirements without
/// modifying outside files.
pub trait ReleaseListener<L> {
    type Error: std::error::Error + Send + Sync + 'static;
    fn release_listener(
        &self,
        listener: L,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send;
}

pub struct ServerRuntime<P>
where
    P: ProvideListener + ReleaseListener<P::Listener>,
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
    P: ProvideListener + ReleaseListener<P::Listener> + Send + Sync + 'static,
    P::Listener: h3x::quic::Listen + Send + 'static,
    <P::Listener as h3x::quic::Listen>::Error: Send,
    <P::Listener as h3x::quic::Listen>::Connection: Send + 'static,
    <<P::Listener as h3x::quic::Listen>::Connection as h3x::quic::WithLocalAgent>::LocalAgent:
        Send + Sync,
    <<P::Listener as h3x::quic::Listen>::Connection as h3x::quic::WithRemoteAgent>::RemoteAgent:
        Send + Sync,
{
    pub fn start(
        source: WorkerServerSource,
        prepared: PreparedServerUpdate,
        plane: Arc<P>,
        listener: P::Listener,
    ) -> Self {
        Self {
            name: prepared.name,
            source: ServerSource::Worker(source),
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
        new_source: WorkerServerSource,
        ctx: &WorkerPrepareContext,
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

        self.apply_reload(ServerSource::Worker(new_source), prepared)
            .await
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
            if let Err(error) = self.plane.release_listener(listener).await {
                tracing::error!(
                    server_name = %self.name,
                    error = %Report::from_error(&error),
                    "failed to release listener to control plane during removal"
                );
            }
        }
    }

    async fn stop_accept(&mut self) -> Option<P::Listener> {
        let old = std::mem::replace(&mut self.accept, AcceptState::Transitioning);
        match old.into_listener().await {
            Ok(listener) => Some(listener),
            Err(_) => None,
        }
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

    impl h3x::quic::Listen for FakeListener {
        type Connection = h3x::dquic::prelude::Connection;
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

    impl ReleaseListener<FakeListener> for FakePlane {
        type Error = FakeRebuildError;
        async fn release_listener(&self, _listener: FakeListener) -> Result<(), Self::Error> {
            Ok(())
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
        struct FailingPlane;
        #[derive(Debug, snafu::Snafu)]
        #[snafu(display("fake release error"))]
        struct FakeReleaseError;

        impl gateway::control_plane::ProvideListener for FailingPlane {
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

        impl ReleaseListener<FakeListener> for FailingPlane {
            type Error = FakeReleaseError;
            async fn release_listener(&self, _listener: FakeListener) -> Result<(), Self::Error> {
                Err(FakeReleaseError)
            }
        }
        let runtime = ServerRuntime {
            name: DhttpName::try_from("alpha.example".to_owned()).unwrap(),
            source: ServerSource::fake_success("alpha.example", 1, ListenerSpec::fake("same")),
            listener_spec: ListenerSpec::fake("same"),
            service: Arc::new(ServerService::fake()),
            accept: AcceptState::Stopped {
                listener: FakeListener,
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
