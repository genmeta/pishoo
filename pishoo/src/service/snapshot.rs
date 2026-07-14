use std::{future::Future, sync::Arc};

use axum::middleware::from_fn_with_state;
use dhttp::h3x::{
    connection::ConnectionBuilder, dhttp::settings::Settings, endpoint::H3Endpoint,
    hyper::TowerService, quic,
};
use gateway::reverse::{
    access_control::{AccessControlState, access_control},
    access_log::{AccessLogState, ActiveAccessLog, access_log},
    body_adapter::BodyAdapterLayer,
    location::ConfiguredLocation,
    router::NginxRouter,
};
use snafu::Report;
use tokio_util::sync::CancellationToken;
use tower::ServiceBuilder;
use tracing::Instrument;

use super::resource::AccessLogResources;

pub struct PreparedServerService {
    pub h3_settings: Arc<Settings>,
    pub access_rules: Arc<dyn dhttp::access::policy::LocationRuleEvaluator + Send + Sync>,
    pub router_state: gateway::reverse::router::RouterState,
    pub server_config: Arc<gateway::parse::config::ServerConfig>,
    pub server_name: dhttp::name::DhttpName<'static>,
}

impl PreparedServerService {
    pub fn activate(self, access_logs: AccessLogResources) -> ServerService {
        ServerService {
            h3_settings: self.h3_settings,
            access_rules: self.access_rules,
            router_state: self.router_state,
            server_config: self.server_config,
            server_name: self.server_name,
            access_logs,
        }
    }
}

pub struct ServerService {
    pub h3_settings: Arc<Settings>,
    pub access_rules: Arc<dyn dhttp::access::policy::LocationRuleEvaluator + Send + Sync>,
    pub router_state: gateway::reverse::router::RouterState,
    pub server_config: Arc<gateway::parse::config::ServerConfig>,
    pub server_name: dhttp::name::DhttpName<'static>,
    pub access_logs: AccessLogResources,
}

impl ServerService {
    pub fn serve_until_shutdown<L>(
        self: Arc<Self>,
        listener: L,
        shutdown: CancellationToken,
    ) -> impl Future<Output = L> + Send
    where
        L: quic::Listen + Send + 'static,
        L::Error: Send,
        L::Connection: Send + 'static,
        <L::Connection as quic::WithLocalAuthority>::LocalAuthority: Send + Sync,
        <L::Connection as quic::WithRemoteAuthority>::RemoteAuthority: Send + Sync,
    {
        assert_eq!(
            self.server_config.locations().len(),
            self.access_logs.locations.len(),
            "access log resources correspond to every configured location"
        );
        let locations = self
            .server_config
            .locations()
            .iter()
            .cloned()
            .zip(self.access_logs.locations.iter().cloned())
            .map(|(location, output)| {
                Arc::new(ConfiguredLocation::new(
                    Arc::new(location),
                    ActiveAccessLog::from_output(output),
                ))
            })
            .collect();

        let server_access_log = ActiveAccessLog::from_output(self.access_logs.server.clone());
        let nginx_router = NginxRouter::new(
            locations,
            server_access_log.clone(),
            self.router_state.clone(),
        );
        let access_state = AccessControlState {
            access_rules: self.access_rules.clone(),
            server_name: Arc::from(self.server_name.as_full()),
        };
        let access_log_state = AccessLogState {
            server: server_access_log,
        };

        let service_stack = ServiceBuilder::new()
            .layer(BodyAdapterLayer)
            .layer(from_fn_with_state(access_log_state, access_log))
            .layer(from_fn_with_state(access_state, access_control))
            .service(nginx_router);

        let builder = ConnectionBuilder::new(self.h3_settings.clone());
        #[cfg(feature = "sshd")]
        let builder = builder.protocol(dhttp::h3x::webtransport::WebTransportProtocolFactory);

        let mut endpoint = H3Endpoint::builder()
            .quic(listener)
            .builder(Arc::new(builder))
            .build();
        let service = TowerService(service_stack);

        let server_name = self.server_name.clone();

        async move {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => {
                    // Intentionally not calling endpoint.shutdown() —
                    // preserve the underlying QUIC listener bindings.
                }
                result = endpoint.listen(service) => {
                    match result {
                        Ok(()) => {
                            tracing::warn!(%server_name, "server stopped");
                        }
                        Err(error) => {
                            tracing::warn!(
                                %server_name,
                                error = %Report::from_error(&error),
                                "server stopped"
                            );
                        }
                    }
                }
            }
            endpoint.into_quic()
        }
        .in_current_span()
    }
}

impl<L> super::accept::AcceptDriver<L> for ServerService
where
    L: quic::Listen + Send + 'static,
    L::Error: Send,
    L::Connection: Send + 'static,
    <L::Connection as quic::WithLocalAuthority>::LocalAuthority: Send + Sync,
    <L::Connection as quic::WithRemoteAuthority>::RemoteAuthority: Send + Sync,
{
    fn drive(
        self: Arc<Self>,
        listener: L,
        shutdown: CancellationToken,
    ) -> impl Future<Output = L> + Send {
        self.serve_until_shutdown(listener, shutdown)
    }
}

#[cfg(test)]
impl ServerService {
    pub(crate) fn fake() -> PreparedServerService {
        PreparedServerService {
            h3_settings: Arc::new(Settings::default()),
            access_rules: Arc::new(dhttp::access::matcher::LocationRulesMatcher::default()),
            router_state: {
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
                        unimplemented!()
                    }
                }
                #[cfg(feature = "sshd")]
                struct DummyTaskScope(tokio_util::sync::CancellationToken);
                #[cfg(feature = "sshd")]
                impl gateway::reverse::router::DynTaskScope for DummyTaskScope {
                    fn spawn(
                        &self,
                        _: std::pin::Pin<
                            Box<dyn std::future::Future<Output = ()> + Send + 'static>,
                        >,
                    ) {
                        unimplemented!()
                    }
                    fn token(&self) -> tokio_util::sync::CancellationToken {
                        self.0.clone()
                    }
                }

                gateway::reverse::router::RouterState {
                    #[cfg(feature = "sshd")]
                    session_spawner: Arc::new(DummySpawner),
                    #[cfg(feature = "sshd")]
                    task_scope: Arc::new(
                        DummyTaskScope(tokio_util::sync::CancellationToken::new()),
                    ),
                }
            },
            server_config: {
                let mut parser = gateway::parse::TypedConfigParser::new();
                let parsed = parser.parse_root(
                    "pishoo { server { listen all 443; server_name test.example; ssl_certificate /tmp/test.crt; ssl_certificate_key /tmp/test.key; } }",
                    std::path::Path::new("/tmp/pishoo.conf"),
                    None,
                ).unwrap();
                Arc::new(
                    parsed
                        .into_parts()
                        .1
                        .into_vec()
                        .pop()
                        .unwrap()
                        .into_result()
                        .unwrap(),
                )
            },
            server_name: dhttp::name::DhttpName::try_from("test.example").unwrap(),
        }
    }
}
