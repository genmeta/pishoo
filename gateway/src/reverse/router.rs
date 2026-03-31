use std::{
    convert::Infallible,
    sync::Arc,
    task::{Context, Poll},
};

use axum::{handler::Handler, response::IntoResponse};
use bytes::Bytes;
use futures::future::BoxFuture;
use h3x::message::stream::MessageStreamError;
use http::StatusCode;
use http_body_util::combinators::UnsyncBoxBody;

use crate::parse::Node;

use super::location::match_location;

type ReqBody = UnsyncBoxBody<Bytes, MessageStreamError>;

/// Nginx-style location router implementing `tower::Service`.
///
/// Matches incoming requests against configured location blocks using nginx's
/// priority rules (exact > prefix > regex > normal-prefix > common), injects
/// `LocationMatch` as a request extension, and dispatches to the appropriate
/// handler (proxy, file, or sshd).
#[derive(Clone)]
pub struct NginxRouter {
    locations: Vec<Arc<Node>>,
}

impl NginxRouter {
    pub fn new(locations: Vec<Arc<Node>>) -> Self {
        Self { locations }
    }
}

impl tower_service::Service<http::Request<ReqBody>> for NginxRouter {
    type Response = axum::response::Response;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, mut request: http::Request<ReqBody>) -> Self::Future {
        let locations = self.locations.clone();

        Box::pin(async move {
            let path = request.uri().path().to_string();

            let loc_match = match match_location(&locations, &path) {
                Some(m) => m,
                None => return Ok(StatusCode::NOT_FOUND.into_response()),
            };

            // Inject LocationMatch into request extensions for extractors
            request.extensions_mut().insert(loc_match.clone());

            // Convert body for axum handlers
            let (parts, body) = request.into_parts();
            let request = http::Request::from_parts(parts, axum::body::Body::new(body));

            let location = &loc_match.location;

            let response = if location.get("proxy_pass").is_some() {
                Handler::call(super::proxy::proxy_handle, request, ()).await
            } else if location.get("root").is_some() || location.get("alias").is_some() {
                Handler::call(super::file::file_handle, request, ()).await
            } else {
                #[cfg(feature = "sshd")]
                if location.get("ssh_login").is_some() {
                    return Ok(Handler::call(
                        super::sshd::sshd_handle,
                        request,
                        (),
                    )
                    .await);
                }
                StatusCode::NOT_FOUND.into_response()
            };

            Ok(response)
        })
    }
}
