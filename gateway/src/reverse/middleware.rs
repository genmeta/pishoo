use std::{
    convert::Infallible,
    sync::Arc,
    task::{Context, Poll},
};

use axum::{
    body::Body,
    extract::{Request, State},
    middleware::Next,
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use firewall_base::matcher::{LocationRulesMatcher, MatchRuleFailed};
use futures::future::BoxFuture;
use h3x::{message::stream::MessageStreamError, quic::agent};
use http::StatusCode;
use http_body_util::combinators::UnsyncBoxBody;
use tracing::{info, warn};

use super::log::RequestInfo;

type H3Body = UnsyncBoxBody<Bytes, MessageStreamError>;

// ---------------------------------------------------------------------------
// BodyAdapterLayer
// ---------------------------------------------------------------------------

/// Tower layer that converts the h3x body type (`UnsyncBoxBody<Bytes,
/// MessageStreamError>`) into `axum::body::Body` so that inner layers can use
/// standard axum middleware patterns (`from_fn`, extractors, etc.).
#[derive(Clone, Copy)]
pub struct BodyAdapterLayer;

impl<S> tower::Layer<S> for BodyAdapterLayer {
    type Service = BodyAdapter<S>;

    fn layer(&self, inner: S) -> Self::Service {
        BodyAdapter { inner }
    }
}

#[derive(Clone)]
pub struct BodyAdapter<S> {
    inner: S,
}

impl<S> tower_service::Service<http::Request<H3Body>> for BodyAdapter<S>
where
    S: tower_service::Service<Request, Response = Response, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send,
{
    type Response = Response;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: http::Request<H3Body>) -> Self::Future {
        let mut inner = self.inner.clone();
        Box::pin(async move {
            let (parts, body) = request.into_parts();
            let request = http::Request::from_parts(parts, Body::new(body));
            inner.call(request).await
        })
    }
}

// ---------------------------------------------------------------------------
// AccessControlState
// ---------------------------------------------------------------------------

/// Shared state for the access control middleware.
#[derive(Clone)]
pub struct AccessControlState {
    pub access_rules: Arc<LocationRulesMatcher>,
    pub server_name: Arc<str>,
}

// ---------------------------------------------------------------------------
// access_control middleware
// ---------------------------------------------------------------------------

/// Axum middleware that enforces firewall access control rules.
///
/// When no ruleset matches the request path (`MatchSet`), the request is
/// allowed only if the client is the server itself; all others are denied.
/// When a ruleset matches but no individual rule matches (`MatchRuleInSet`),
/// the request is denied for everyone.
pub async fn access_control(
    State(state): State<AccessControlState>,
    request: Request,
    next: Next,
) -> Response {
    let client_name = request
        .extensions()
        .get::<Arc<dyn agent::RemoteAgent>>()
        .map(|a| a.name().to_owned());
    let http_request =
        firewall_base::expr::atomics::HttpRequest::new(client_name.as_deref(), &request);

    let action = match state
        .access_rules
        .match_rule(request.uri().path(), &http_request)
    {
        Ok((_location, action)) => action,
        Err(MatchRuleFailed::MatchSet { .. }) => {
            // No ruleset matched the path — allow the server itself, deny others.
            if client_name.as_deref() == Some(&*state.server_name) {
                warn!(
                    path = %request.uri().path(),
                    "no ruleset matched, allowing self only"
                );
                firewall_base::action::RequestAction::Allow
            } else {
                warn!(
                    path = %request.uri().path(),
                    client = client_name.as_deref(),
                    "no ruleset matched, denying non-self client"
                );
                firewall_base::action::RequestAction::Deny
            }
        }
        Err(MatchRuleFailed::MatchRuleInSet) => {
            // Ruleset matched but no rule matched — deny everyone.
            warn!(
                path = %request.uri().path(),
                "ruleset matched but no rule matched, denying all"
            );
            firewall_base::action::RequestAction::Deny
        }
    };

    if action == firewall_base::action::RequestAction::Deny {
        info!(
            client_name = client_name.as_deref(),
            uri = %request.uri(),
            "firewall rules denied request"
        );
        return StatusCode::FORBIDDEN.into_response();
    }

    next.run(request).await
}

// ---------------------------------------------------------------------------
// access_log middleware
// ---------------------------------------------------------------------------

/// Axum middleware that logs every request/response in Combined Log Format.
pub async fn access_log(request: Request, next: Next) -> Response {
    let req_info = RequestInfo::from_request(&request);

    let response = next.run(request).await;

    let status = response.status().as_u16();
    // Content-Length gives us the body size without consuming the body
    let body_size = response
        .headers()
        .get(http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);

    req_info.log_access(status, body_size).await;

    response
}
