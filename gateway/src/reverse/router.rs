use std::{
    convert::Infallible,
    sync::Arc,
    task::{Context, Poll},
};

use axum::{body::Body, handler::Handler, response::IntoResponse};
use futures::future::BoxFuture;
use http::StatusCode;

use super::location::match_location;
#[cfg(feature = "sshd")]
use crate::parse::types::SshLoginMethods;
use crate::parse::{
    document::ConfigNode,
    types::{PathConfig, ProxyPass},
};

/// Shared state for all reverse-proxy handlers.
///
/// Injected as axum `State` into every handler. Currently holds SSH
/// session spawning support; designed for future extensions (e.g.
/// forward proxy connector, WebSocket upgrade).
#[derive(Clone)]
pub struct RouterState {
    #[cfg(feature = "sshd")]
    pub session_spawner: std::sync::Arc<dyn crate::control_plane::DynSpawnSession>,
}

/// Nginx-style location router implementing `tower::Service`.
///
/// Matches incoming requests against configured location blocks using nginx's
/// priority rules (exact > prefix > regex > normal-prefix > common), injects
/// `LocationMatch` as a request extension, and dispatches to the appropriate
/// handler (proxy, file, or sshd).
#[derive(Clone)]
pub struct NginxRouter {
    locations: Vec<Arc<ConfigNode>>,
    state: RouterState,
}

impl NginxRouter {
    pub fn new(locations: Vec<Arc<ConfigNode>>, state: RouterState) -> Self {
        Self { locations, state }
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
        let state = self.state.clone();

        Box::pin(async move {
            let path = request.uri().path().to_string();

            let loc_match = match match_location(&locations, &path) {
                Some(m) => m,
                None => return Ok(StatusCode::NOT_FOUND.into_response()),
            };

            // Inject LocationMatch into request extensions for extractors
            request.extensions_mut().insert(loc_match.clone());

            let location = &loc_match.location;

            let response = if location
                .get::<ProxyPass>("proxy_pass")
                .ok()
                .flatten()
                .is_some()
            {
                Handler::call(super::proxy::proxy_handle, request, state).await
            } else if location.get::<PathConfig>("root").ok().flatten().is_some()
                || location.get::<PathConfig>("alias").ok().flatten().is_some()
            {
                Handler::call(super::file::file_handle, request, state).await
            } else {
                #[cfg(feature = "sshd")]
                if location
                    .get::<SshLoginMethods>("ssh_login")
                    .ok()
                    .flatten()
                    .is_some()
                {
                    return Ok(Handler::call(super::sshd::sshd_handle, request, state).await);
                }
                StatusCode::NOT_FOUND.into_response()
            };

            Ok(response)
        })
    }
}
