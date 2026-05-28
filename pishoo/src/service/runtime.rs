#![allow(dead_code)]

use std::sync::Arc;

use dhttp::name::DhttpName;
use gateway::control_plane::ProvideListener;
use snafu::Report;

use super::{
    accept::AcceptState,
    snapshot::ServerService,
    source::{ListenerSpec, PreparedServerUpdate, ServerFingerprint, ServerSource},
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
    PrepareFailed,
    Reloaded { listener: ReloadedListener },
    StoppedAfterFatalRebuild,
}

pub enum ReloadedListener {
    Reused,
    Rebuilt,
}

impl<P> ServerRuntime<P>
where
    P: ProvideListener + 'static,
{
    pub fn name(&self) -> &DhttpName<'static> {
        &self.name
    }

    pub fn fingerprint(&self) -> &ServerFingerprint {
        &self.fingerprint
    }

    pub async fn reload(&mut self, new_source: ServerSource) -> ReloadServerOutcome {
        let prepared = match new_source.prepare().await {
            Ok(prepared) => prepared,
            Err(error) => {
                tracing::warn!(
                    server_name = %self.name,
                    error = %Report::from_error(&error),
                    "failed to prepare server reload"
                );
                return ReloadServerOutcome::PrepareFailed;
            }
        };

        if prepared.listener_spec == self.listener_spec {
            self.apply_reused_listener(new_source, prepared).await
        } else {
            self.apply_rebuilt_listener(new_source, prepared).await
        }
    }

    async fn apply_reused_listener(
        &mut self,
        new_source: ServerSource,
        prepared: PreparedServerUpdate,
    ) -> ReloadServerOutcome {
        let old = std::mem::replace(&mut self.accept, AcceptState::Transitioning);
        match old.into_listener().await {
            Ok(listener) => {
                self.commit(new_source, prepared);
                self.accept = AcceptState::Stopped { listener };
                ReloadServerOutcome::Reloaded {
                    listener: ReloadedListener::Reused,
                }
            }
            Err(error) => {
                tracing::error!(
                    server_name = %self.name,
                    error = %Report::from_error(&error),
                    "failed to stop accept loop before reuse"
                );
                ReloadServerOutcome::StoppedAfterFatalRebuild
            }
        }
    }

    async fn apply_rebuilt_listener(
        &mut self,
        new_source: ServerSource,
        prepared: PreparedServerUpdate,
    ) -> ReloadServerOutcome {
        let old = std::mem::replace(&mut self.accept, AcceptState::Transitioning);
        let old_listener = match old.into_listener().await {
            Ok(listener) => listener,
            Err(error) => {
                tracing::error!(
                    server_name = %self.name,
                    error = %Report::from_error(&error),
                    "failed to recover listener before rebuild"
                );
                return ReloadServerOutcome::StoppedAfterFatalRebuild;
            }
        };

        let request = prepared
            .listen_request
            .clone()
            .expect("production prepare must populate listen_request");

        match self.plane.rebuild_listener(old_listener, request).await {
            Ok(new_listener) => {
                self.commit(new_source, prepared);
                self.accept = AcceptState::Stopped {
                    listener: new_listener,
                };
                ReloadServerOutcome::Reloaded {
                    listener: ReloadedListener::Rebuilt,
                }
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

    fn commit(&mut self, new_source: ServerSource, prepared: PreparedServerUpdate) {
        self.source = new_source;
        self.listener_spec = prepared.listener_spec;
        self.fingerprint = prepared.fingerprint;
        self.service = prepared.service;
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

        let outcome = runtime.reload(failing_source).await;

        assert!(matches!(outcome, ReloadServerOutcome::PrepareFailed));
        assert_eq!(runtime.fingerprint().generation_for_test(), 1);
    }

    #[tokio::test]
    async fn unchanged_listener_reload_swaps_service_for_future_accepts() {
        let mut runtime = fake_runtime("alpha.example", 1);
        let source = ServerSource::fake_success("alpha.example", 2, ListenerSpec::fake("same"));

        let outcome = runtime.reload(source).await;

        assert!(matches!(
            outcome,
            ReloadServerOutcome::Reloaded {
                listener: ReloadedListener::Reused
            }
        ));
        assert_eq!(runtime.fingerprint().generation_for_test(), 2);
    }
}
