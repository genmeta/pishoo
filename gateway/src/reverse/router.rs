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

use super::location::match_location;
#[cfg(feature = "sshd")]
use crate::parse::types::SshLoginMethods;
use crate::parse::{
    document::ConfigNode, domain::ResolvedConfigPath, pattern::Pattern, types::ProxyPass,
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
            let normalized = super::request_uri::normalize_request_uri(request.uri())
                .expect("request uri should always normalize");
            let public_origin = super::request_uri::request_public_origin(&request);

            if !has_exact_match(&locations, &normalized.path)
                && let Some(target) = find_proxy_prefix_slash_redirect(
                    &locations,
                    &normalized,
                    public_origin.as_deref(),
                )
            {
                return Ok(
                    (StatusCode::MOVED_PERMANENTLY, [(header::LOCATION, target)]).into_response(),
                );
            }

            let loc_match = match match_location(&locations, &normalized.path) {
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
            } else if location
                .get::<ResolvedConfigPath>("root")
                .ok()
                .flatten()
                .is_some()
                || location
                    .get::<ResolvedConfigPath>("alias")
                    .ok()
                    .flatten()
                    .is_some()
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

fn has_exact_match(locations: &[Arc<ConfigNode>], path: &str) -> bool {
    locations.iter().any(|location| {
        location.payload::<Pattern>().ok().flatten().is_some_and(
            |pattern| matches!(pattern.as_ref(), Pattern::Exact(expected) if expected == path),
        )
    })
}

fn find_proxy_prefix_slash_redirect(
    locations: &[Arc<ConfigNode>],
    normalized: &super::request_uri::NormalizedRequestUri,
    public_origin: Option<&str>,
) -> Option<String> {
    let mut best: Option<(usize, String)> = None;

    for location in locations {
        let has_proxy = location
            .get::<ProxyPass>("proxy_pass")
            .ok()
            .flatten()
            .is_some();
        if !has_proxy {
            continue;
        }

        let Some(pattern) = location.payload::<Pattern>().ok().flatten() else {
            continue;
        };

        let prefix = match pattern.as_ref() {
            Pattern::Prefix(prefix) | Pattern::NormalPrefix(prefix) => prefix,
            _ => continue,
        };

        let Some(target) =
            super::request_uri::build_prefix_slash_redirect(prefix, normalized, public_origin)
        else {
            continue;
        };

        if best
            .as_ref()
            .is_none_or(|(current_len, _)| prefix.len() > *current_len)
        {
            best = Some((prefix.len(), target));
        }
    }

    best.map(|(_, target)| target)
}
