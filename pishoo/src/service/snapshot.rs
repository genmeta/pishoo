use std::{future::Future, path::PathBuf, sync::Arc};

use axum::middleware::from_fn_with_state;
use gateway::reverse::{
    access_control::{AccessControlState, access_control},
    access_log::{AccessLogState, access_log},
    body_adapter::BodyAdapterLayer,
    log::AccessLogWriter,
    router::NginxRouter,
};
use h3x::{
    connection::ConnectionBuilder, dhttp::settings::Settings, endpoint::H3Endpoint,
    hyper::server::TowerService, quic,
};
use snafu::Report;
use tokio_util::sync::CancellationToken;
use tower::ServiceBuilder;
use tracing::Instrument;

pub struct ServerService {
    pub h3_settings: Arc<Settings>,
    pub access_rules: Arc<dhttp_access::db::base::matcher::LocationRulesMatcher>,
    pub router_state: gateway::reverse::router::RouterState,
    pub server_node: Arc<gateway::parse::document::ConfigNode>,
    pub access_log_dir: Option<PathBuf>,
    pub server_name: dhttp::name::DhttpName<'static>,
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
        <L::Connection as quic::WithLocalAgent>::LocalAgent: Send + Sync,
        <L::Connection as quic::WithRemoteAgent>::RemoteAgent: Send + Sync,
    {
        let locations = self.server_node.children_optional("location").to_vec();

        let nginx_router = NginxRouter::new(locations, self.router_state.clone());
        let access_state = AccessControlState {
            access_rules: self.access_rules.clone(),
            server_name: Arc::from(self.server_name.as_full()),
        };

        let access_log_writer = match &self.access_log_dir {
            Some(dir) => match AccessLogWriter::new(dir.clone()) {
                Ok(writer) => {
                    tracing::debug!(
                        server_name = %self.server_name,
                        dir = %dir.display(),
                        "access log writer created"
                    );
                    writer
                }
                Err(error) => {
                    tracing::warn!(
                        server_name = %self.server_name,
                        error = %Report::from_error(&error),
                        "failed to create access log writer, access logging disabled"
                    );
                    AccessLogWriter::disabled()
                }
            },
            None => AccessLogWriter::disabled(),
        };
        let access_log_state = AccessLogState {
            writer: access_log_writer,
        };

        let service_stack = ServiceBuilder::new()
            .layer(BodyAdapterLayer)
            .layer(from_fn_with_state(access_log_state, access_log))
            .layer(from_fn_with_state(access_state, access_control))
            .service(nginx_router);

        let builder = ConnectionBuilder::new(self.h3_settings.clone());
        #[cfg(feature = "sshd")]
        let builder = builder.protocol(h3x::webtransport::WebTransportProtocolFactory);

        let mut endpoint = H3Endpoint::builder()
            .quic(listener)
            .builder(Arc::new(builder))
            .build();
        let service = TowerService(service_stack);

        let server_name = self.server_name.clone();

        async move {
            tokio::select! {
                result = endpoint.serve(service) => {
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
                () = shutdown.cancelled() => {
                    // Intentionally not calling endpoint.shutdown() —
                    // preserve the underlying QUIC listener bindings.
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
    <L::Connection as quic::WithLocalAgent>::LocalAgent: Send + Sync,
    <L::Connection as quic::WithRemoteAgent>::RemoteAgent: Send + Sync,
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
    pub(crate) fn fake() -> Self {
        Self {
            h3_settings: Arc::new(Settings::default()),
            access_rules: Arc::new(
                dhttp_access::db::base::matcher::LocationRulesMatcher::default(),
            ),
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
            server_node: {
                let registry = gateway::parse::default_registry();
                let doc = gateway::parse::load_config_text(
                    "",
                    None,
                    &registry,
                    gateway::parse::registry::BuildOptions {
                        identity_profile: None,
                    },
                )
                .unwrap();
                doc.root
            },
            access_log_dir: None,
            server_name: dhttp::name::DhttpName::try_from("test.example").unwrap(),
        }
    }
}
