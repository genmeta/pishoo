use std::{
    convert::Infallible,
    sync::Arc,
    task::{Context, Poll},
};

use axum::{body::Body, handler::Handler, response::IntoResponse};
use futures::future::BoxFuture;
use http::{StatusCode, header};
#[cfg(feature = "sshd")]
use tokio_util::sync::CancellationToken;

use super::{
    access_log::ActiveAccessLog,
    location::{ConfiguredLocation, match_location},
};
#[cfg(feature = "sshd")]
pub trait DynTaskScope: Send + Sync {
    fn token(&self) -> CancellationToken;

    fn spawn(&self, task: BoxFuture<'static, ()>);
}

/// Shared state for all reverse-proxy handlers.
///
/// Injected as axum `State` into every handler. Currently holds SSH
/// session spawning support; designed for future extensions (e.g.
/// forward proxy connector, WebSocket upgrade).
#[derive(Clone)]
pub struct RouterState {
    #[cfg(feature = "sshd")]
    pub session_spawner: std::sync::Arc<dyn crate::control_plane::DynSpawnSession>,
    #[cfg(feature = "sshd")]
    pub task_scope: Arc<dyn DynTaskScope>,
}

/// Nginx-style location router implementing `tower::Service`.
///
/// Matches incoming requests against configured location blocks using nginx's
/// priority rules (exact > prefix > regex > normal-prefix > common), injects
/// `LocationMatch` as a request extension, and dispatches to the appropriate
/// handler (proxy, file, or sshd).
#[derive(Clone)]
pub struct NginxRouter {
    locations: Vec<Arc<ConfiguredLocation>>,
    server_access_log: ActiveAccessLog,
    state: RouterState,
}

impl NginxRouter {
    pub fn new(
        locations: Vec<Arc<ConfiguredLocation>>,
        server_access_log: ActiveAccessLog,
        state: RouterState,
    ) -> Self {
        Self {
            locations,
            server_access_log,
            state,
        }
    }
}

impl tower_service::Service<http::Request<Body>> for NginxRouter {
    type Response = axum::response::Response;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, mut request: http::Request<Body>) -> Self::Future {
        let locations = self.locations.clone();
        let server_access_log = self.server_access_log.clone();
        let state = self.state.clone();

        Box::pin(async move {
            let normalized = super::request_uri::normalize_request_uri(request.uri())
                .expect("request uri should always normalize");
            let public_origin = super::request_uri::request_public_origin(&request);

            let loc_match = match match_location(&locations, &normalized.path) {
                Some(m) => m,
                None => {
                    let mut response = StatusCode::NOT_FOUND.into_response();
                    response.extensions_mut().insert(server_access_log);
                    return Ok(response);
                }
            };
            let active_access_log = loc_match.access_log.clone();

            if let Some(target) =
                proxy_prefix_slash_redirect(&loc_match, &normalized, public_origin.as_deref())
            {
                let mut response =
                    (StatusCode::MOVED_PERMANENTLY, [(header::LOCATION, target)]).into_response();
                response.extensions_mut().insert(active_access_log);
                return Ok(response);
            }

            // Inject LocationMatch into request extensions for extractors
            request.extensions_mut().insert(loc_match.clone());

            let location = &loc_match.location;

            let response = if location.proxy_pass().is_some() {
                Handler::call(super::proxy::proxy_handle, request, state).await
            } else if location.root().is_some() || location.alias().is_some() {
                Handler::call(super::file::file_handle, request, state).await
            } else {
                #[cfg(feature = "sshd")]
                if location.ssh_login().is_some() {
                    let mut response =
                        Handler::call(super::sshd::sshd_handle, request, state).await;
                    response.extensions_mut().insert(active_access_log);
                    return Ok(response);
                }
                StatusCode::NOT_FOUND.into_response()
            };

            let mut response = response;
            response.extensions_mut().insert(active_access_log);
            Ok(response)
        })
    }
}

fn proxy_prefix_slash_redirect(
    route: &super::location::LocationMatch,
    normalized: &super::request_uri::NormalizedRequestUri,
    public_origin: Option<&str>,
) -> Option<String> {
    route.location.proxy_pass()?;
    let prefix = match route.location.matcher() {
        crate::parse::pattern::Pattern::Prefix(prefix)
        | crate::parse::pattern::Pattern::NormalPrefix(prefix) => prefix,
        _ => return None,
    };
    super::request_uri::build_prefix_slash_redirect(prefix, normalized, public_origin)
}

#[cfg(test)]
mod tests {
    use tower::ServiceExt;

    use super::*;
    use crate::{
        parse::{domain::ResolvedConfigPath, tests::parse_location},
        reverse::{location::ConfiguredLocation, log::AccessLogOutput},
    };

    fn router_state() -> RouterState {
        RouterState {
            #[cfg(feature = "sshd")]
            session_spawner: Arc::new(DummySpawner),
            #[cfg(feature = "sshd")]
            task_scope: Arc::new(DummyScope),
        }
    }

    #[cfg(feature = "sshd")]
    struct DummySpawner;

    #[cfg(feature = "sshd")]
    impl crate::control_plane::DynSpawnSession for DummySpawner {
        fn spawn_session<'a>(
            &'a self,
            _username: &'a str,
        ) -> BoxFuture<
            'a,
            Result<
                crate::control_plane::SessionTransport,
                Box<dyn std::error::Error + Send + Sync>,
            >,
        > {
            Box::pin(std::future::pending())
        }
    }

    #[cfg(feature = "sshd")]
    struct DummyScope;

    #[cfg(feature = "sshd")]
    impl DynTaskScope for DummyScope {
        fn token(&self) -> CancellationToken {
            CancellationToken::new()
        }

        fn spawn(&self, _task: BoxFuture<'static, ()>) {}
    }

    #[tokio::test]
    async fn selected_location_hands_its_output_to_the_response() {
        let path = std::env::temp_dir().join(format!(
            "gateway-route-access-{}-{}.log",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let output = Arc::new(
            AccessLogOutput::open(ResolvedConfigPath::try_from(path.clone()).unwrap()).unwrap(),
        );
        let location = Arc::new(ConfiguredLocation::new(
            parse_location("").unwrap(),
            ActiveAccessLog::Enabled(output.clone()),
        ));
        let router = NginxRouter::new(vec![location], ActiveAccessLog::Disabled, router_state());

        let response = router
            .oneshot(
                http::Request::builder()
                    .uri("/anything")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let Some(ActiveAccessLog::Enabled(selected)) =
            response.extensions().get::<ActiveAccessLog>()
        else {
            panic!("selected route did not attach its access output");
        };
        assert!(Arc::ptr_eq(selected, &output));
        drop(response);
        drop(output);
        let _ = std::fs::remove_file(path);
    }
}
