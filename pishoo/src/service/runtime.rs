use std::{collections::HashSet, sync::Arc};

use dhttp::{h3x::quic::Listen as _, name::DhttpName};
use gateway::control_plane::{ControlPlane, ProvideListener};
use snafu::{Report, ResultExt};

use super::{
    accept::DrainOutcome,
    resource::ServerResources,
    set::{ResourceSet, ServerServiceHandle, ServiceSet},
    source::{PrepareContext, ServerSource},
};

pub struct RuntimeRegistry<P>
where
    P: ProvideListener,
{
    plane: Arc<P>,
    resources: ResourceSet<P::Listener>,
    services: ServiceSet<P::Listener>,
}

impl<P> RuntimeRegistry<P>
where
    P: ProvideListener + Send + Sync + 'static,
    P::Listener: dhttp::h3x::quic::Listen + Send + 'static,
    <P::Listener as dhttp::h3x::quic::Listen>::Error: std::error::Error + Send + Sync + 'static,
    <P::Listener as dhttp::h3x::quic::Listen>::Connection: Send + 'static,
    <<P::Listener as dhttp::h3x::quic::Listen>::Connection as dhttp::h3x::quic::WithLocalAuthority>::LocalAuthority: Send + Sync,
    <<P::Listener as dhttp::h3x::quic::Listen>::Connection as dhttp::h3x::quic::WithRemoteAuthority>::RemoteAuthority: Send + Sync,
{
    pub fn new(plane: Arc<P>) -> Self {
        Self { plane, resources: ResourceSet::default(), services: ServiceSet::default() }
    }

    pub async fn apply_sources(&mut self, sources: Vec<ServerSource>, ctx: &PrepareContext) {
        self.stop_finished_services().await;
        let desired = sources.iter().map(|source| source.name().clone()).collect::<HashSet<_>>();
        let removed = self.resources.servers.keys().filter(|name| !desired.contains(*name)).cloned().collect::<Vec<_>>();
        for name in removed { self.stop_server(&name).await; }
        for source in sources { self.reconcile_server(source, ctx).await; }
    }

    async fn reconcile_server(&mut self, source: ServerSource, ctx: &PrepareContext) {
        let name = source.name().clone();
        self.stop_service(&name).await;

        let prepared = match source.prepare(ctx).await {
            Ok(prepared) => prepared,
            Err(error) => {
                tracing::warn!(server_name = %name, error = %Report::from_error(&error), "server service preparation failed");
                self.release_resource(&name).await;
                return;
            }
        };

        let access_logs = match self.resources.acquire_access_logs(prepared.access_logs) {
            Ok(access_logs) => access_logs,
            Err(error) => {
                tracing::warn!(
                    server_name = %name,
                    error = %Report::from_error(&error),
                    "server access log resource acquisition failed"
                );
                self.release_resource(&name).await;
                return;
            }
        };

        let reusable = self.resources.servers.get(&name)
            .is_some_and(|resources| resources.listener_spec() == &prepared.listener_spec);
        if !reusable { self.release_resource(&name).await; }

        if !self.resources.servers.contains_key(&name) {
            let listener = match self.plane.listener(prepared.listen_request.clone()).await {
                Ok(listener) => listener,
                Err(error) => {
                    tracing::error!(server_name = %name, error = %Report::from_error(&error), "server listener acquisition failed");
                    return;
                }
            };
            self.resources.servers.insert(
                name.clone(),
                ServerResources::new(listener, prepared.listener_spec.clone(), access_logs),
            );
        } else {
            self.resources
                .servers
                .get_mut(&name)
                .expect("reusable resource exists")
                .replace_access_logs(access_logs);
        }

        let listener = self.resources.servers.get_mut(&name).expect("resource inserted").take_listener();
        let service = Arc::new(prepared.service.activate(
            self.resources
                .servers
                .get(&name)
                .expect("resource inserted")
                .access_logs()
                .clone(),
        ));
        let completed = self.services.completion_sender();
        self.services.servers.insert(
            name.clone(),
            ServerServiceHandle::start(name, listener, service, completed),
        );
    }

    async fn stop_finished_services(&mut self) {
        let finished = self
            .services
            .servers
            .iter()
            .filter(|(_, service)| service.is_finished())
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        for name in finished {
            tracing::warn!(server_name = %name, "server service exited unexpectedly");
            self.stop_server(&name).await;
        }
    }

    pub async fn wait_service_completion(&mut self) -> DhttpName<'static> {
        loop {
            let completion = self.services.next_completed().await;
            if self
                .services
                .servers
                .get(&completion.name)
                .is_some_and(|service| service.owns_completion(&completion))
            {
                return completion.name;
            }
        }
    }

    pub async fn handle_service_exit(&mut self, name: DhttpName<'static>) {
        tracing::warn!(server_name = %name, "server service exited unexpectedly");
        self.stop_server(&name).await;
    }

    async fn stop_service(&mut self, name: &DhttpName<'static>) {
        let Some(service) = self.services.servers.remove(name) else { return };
        match service.drain().await {
            DrainOutcome::Returned(listener) => {
                if let Some(resources) = self.resources.servers.get_mut(name) {
                    resources.put_listener(listener);
                } else {
                    let _ = listener.shutdown().await;
                }
            }
            DrainOutcome::Aborted => {
                self.resources.servers.remove(name);
            }
        }
    }

    async fn release_resource(&mut self, name: &DhttpName<'static>) {
        let Some(mut resources) = self.resources.servers.remove(name) else { return };
        let listener = resources.take_listener();
        if let Err(error) = listener.shutdown().await {
            tracing::warn!(server_name = %name, error = %Report::from_error(&error), "server listener shutdown failed");
        }
    }

    async fn stop_server(&mut self, name: &DhttpName<'static>) {
        self.stop_service(name).await;
        self.release_resource(name).await;
    }

    pub async fn shutdown(mut self) {
        let names = self.resources.servers.keys().cloned().collect::<Vec<_>>();
        for name in names { self.stop_server(&name).await; }
    }

    #[cfg(test)]
    pub(crate) fn contains_service(&self, name: &str) -> bool {
        self.services.servers.keys().any(|candidate| candidate.as_full() == name)
    }
}

#[derive(Debug, snafu::Snafu)]
#[snafu(module(worker_reload_error))]
pub enum WorkerReloadError {
    #[snafu(display("failed to load worker configuration"))]
    Config {
        source: gateway::parse::error::ConfigLoadFailure,
    },
    #[snafu(display("failed to enumerate worker identities"))]
    Identities {
        source: crate::config::plan::LoadIdentityServerCandidatesError,
    },
}

pub struct WorkerRuntime<P>
where
    P: ControlPlane + ProvideListener,
{
    registry: RuntimeRegistry<P>,
    dhttp_home: dhttp::home::DhttpHome,
    root_defaults: gateway::parse::config::RootWorkerDefaultsSnapshot,
    router_state: gateway::reverse::router::RouterState,
}

impl<P> WorkerRuntime<P>
where
    P: ControlPlane + ProvideListener + Send + Sync + 'static,
    P::Listener: dhttp::h3x::quic::Listen + Send + 'static,
    <P::Listener as dhttp::h3x::quic::Listen>::Error: std::error::Error + Send + Sync + 'static,
    <P::Listener as dhttp::h3x::quic::Listen>::Connection: Send + 'static,
    <<P::Listener as dhttp::h3x::quic::Listen>::Connection as dhttp::h3x::quic::WithLocalAuthority>::LocalAuthority: Send + Sync,
    <<P::Listener as dhttp::h3x::quic::Listen>::Connection as dhttp::h3x::quic::WithRemoteAuthority>::RemoteAuthority: Send + Sync,
{
    pub fn new(plane: Arc<P>, dhttp_home: dhttp::home::DhttpHome, root_defaults: gateway::parse::config::RootWorkerDefaultsSnapshot, router_state: gateway::reverse::router::RouterState) -> Self {
        Self { registry: RuntimeRegistry::new(plane), dhttp_home, root_defaults, router_state }
    }

    pub async fn start(&mut self) -> Result<(), WorkerReloadError> { self.reload().await }

    pub async fn reload(&mut self) -> Result<(), WorkerReloadError> {
        let mut parser = gateway::parse::TypedConfigParser::new();
        let path = self.dhttp_home.join(crate::config::PishooConfigSource::CONFIG_FILE_NAME);
        let defaults = match gateway::parse::load_worker_config_file(&mut parser, &path, &self.dhttp_home, &self.root_defaults).await.context(worker_reload_error::ConfigSnafu)? {
            Some(parsed) => parsed.pishoo().worker_defaults(),
            None => self.root_defaults.clone(),
        };
        let candidates = crate::config::load_identity_server_candidates(&self.dhttp_home, &defaults).await.context(worker_reload_error::IdentitiesSnafu)?;
        let mut configs = Vec::new();
        for candidate in candidates.into_vec() {
            let (profile, result) = candidate.into_parts();
            match result {
                Ok(Some(server)) => configs.push(Arc::new(server)),
                Ok(None) => {}
                Err(error) => tracing::warn!(profile = ?profile.map(|profile| profile.name().to_string()), error = %Report::from_error(&error), "identity server config rejected"),
            }
        }
        let (sources, ctx) = crate::service::source::TypedServerSource::load_all(configs, self.router_state.clone()).await;
        self.registry.apply_sources(sources, &ctx).await;
        Ok(())
    }

    pub async fn reload_with_root_defaults(&mut self, root_defaults: gateway::parse::config::RootWorkerDefaultsSnapshot) -> Result<(), WorkerReloadError> {
        self.root_defaults = root_defaults;
        self.reload().await
    }

    pub async fn shutdown(self) { self.registry.shutdown().await; }

    pub async fn wait_service_completion(&mut self) -> DhttpName<'static> {
        self.registry.wait_service_completion().await
    }

    pub async fn handle_service_exit(&mut self, name: DhttpName<'static>) {
        self.registry.handle_service_exit(name).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use snafu::Snafu;

    use super::*;
    use crate::service::source::{ListenerSpec, ServerSource};

    struct FakeListener {
        operations: Arc<Mutex<Vec<&'static str>>>,
    }
    #[derive(Debug, Snafu)]
    #[snafu(display("fake listener error"))]
    struct FakeListenerError;
    impl dhttp::h3x::quic::Listen for FakeListener {
        type Connection = dhttp::h3x::dquic::prelude::Connection;
        type Error = FakeListenerError;
        async fn accept(&mut self) -> Result<Arc<Self::Connection>, Self::Error> {
            std::future::pending().await
        }
        async fn shutdown(&self) -> Result<(), Self::Error> {
            self.operations.lock().unwrap().push("shutdown");
            Ok(())
        }
    }

    struct FakePlane {
        operations: Arc<Mutex<Vec<&'static str>>>,
    }
    impl gateway::control_plane::ProvideListener for FakePlane {
        type Listener = FakeListener;
        type ListenError = FakeListenerError;
        async fn listener(
            &self,
            _request: gateway::control_plane::ListenRequest,
        ) -> Result<Self::Listener, Self::ListenError> {
            self.operations.lock().unwrap().push("acquire");
            Ok(FakeListener {
                operations: self.operations.clone(),
            })
        }
    }

    fn context() -> PrepareContext {
        PrepareContext {
            h3_settings: Arc::new(dhttp::h3x::dhttp::settings::Settings::default()),
            router_state: gateway::reverse::router::RouterState {
                #[cfg(feature = "sshd")]
                session_spawner: Arc::new(DummySpawner),
                #[cfg(feature = "sshd")]
                task_scope: Arc::new(DummyScope),
            },
        }
    }

    #[cfg(feature = "sshd")]
    struct DummySpawner;
    #[cfg(feature = "sshd")]
    impl gateway::control_plane::DynSpawnSession for DummySpawner {
        fn spawn_session<'a>(
            &'a self,
            _username: &'a str,
        ) -> futures::future::BoxFuture<
            'a,
            Result<
                gateway::control_plane::SessionTransport,
                Box<dyn std::error::Error + Send + Sync>,
            >,
        > {
            Box::pin(async { std::future::pending().await })
        }
    }
    #[cfg(feature = "sshd")]
    struct DummyScope;
    #[cfg(feature = "sshd")]
    impl gateway::reverse::router::DynTaskScope for DummyScope {
        fn token(&self) -> tokio_util::sync::CancellationToken {
            tokio_util::sync::CancellationToken::new()
        }
        fn spawn(&self, _task: futures::future::BoxFuture<'static, ()>) {}
    }

    #[tokio::test]
    async fn changed_listener_drains_closes_and_reacquires_in_one_round() {
        let operations = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = RuntimeRegistry::new(Arc::new(FakePlane {
            operations: operations.clone(),
        }));
        runtime
            .apply_sources(
                vec![ServerSource::fake_success(
                    "alpha.dhttp.net",
                    1,
                    ListenerSpec::fake("a"),
                )],
                &context(),
            )
            .await;
        runtime
            .apply_sources(
                vec![ServerSource::fake_success(
                    "alpha.dhttp.net",
                    2,
                    ListenerSpec::fake("b"),
                )],
                &context(),
            )
            .await;

        assert_eq!(
            *operations.lock().unwrap(),
            ["acquire", "shutdown", "acquire"]
        );
        assert!(runtime.contains_service("alpha.dhttp.net"));
    }

    #[tokio::test]
    async fn preparation_failure_stops_only_that_server() {
        let operations = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = RuntimeRegistry::new(Arc::new(FakePlane { operations }));
        runtime
            .apply_sources(
                vec![
                    ServerSource::fake_success("good.dhttp.net", 1, ListenerSpec::fake("good")),
                    ServerSource::fake_success("bad.dhttp.net", 1, ListenerSpec::fake("bad")),
                ],
                &context(),
            )
            .await;
        runtime
            .apply_sources(
                vec![
                    ServerSource::fake_success("good.dhttp.net", 2, ListenerSpec::fake("good")),
                    ServerSource::fake_prepare_error("bad.dhttp.net"),
                ],
                &context(),
            )
            .await;

        assert!(runtime.contains_service("good.dhttp.net"));
        assert!(!runtime.contains_service("bad.dhttp.net"));
    }
}
