use std::{
    convert::Infallible,
    sync::Arc,
    task::{Context, Poll},
};

use bytes::Bytes;
use futures::future::BoxFuture;
use h3x::{message::stream::MessageStreamError, quic::agent};
use http::StatusCode;
use http_body_util::combinators::UnsyncBoxBody;
use snafu::Report;
use tracing::{info, warn};

use crate::reverse::MissingRulePolicy;

use super::log::RequestInfo;

type ReqBody = UnsyncBoxBody<Bytes, MessageStreamError>;

// ---------------------------------------------------------------------------
// AccessControlLayer
// ---------------------------------------------------------------------------

/// Tower layer that enforces firewall access control rules.
///
/// Extracts the client name from the `RemoteAgent` extension (injected by
/// TowerService), matches the request against the configured firewall rules,
/// and returns 403 Forbidden if denied.
#[derive(Clone)]
pub struct AccessControlLayer {
    access_rules: Arc<firewall_base::matcher::LocationRulesMatcher>,
    missing_rule_policy: MissingRulePolicy,
}

impl AccessControlLayer {
    pub fn new(
        access_rules: Arc<firewall_base::matcher::LocationRulesMatcher>,
        missing_rule_policy: MissingRulePolicy,
    ) -> Self {
        Self {
            access_rules,
            missing_rule_policy,
        }
    }
}

impl<S> tower::Layer<S> for AccessControlLayer {
    type Service = AccessControlService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AccessControlService {
            inner,
            access_rules: self.access_rules.clone(),
            missing_rule_policy: self.missing_rule_policy,
        }
    }
}

#[derive(Clone)]
pub struct AccessControlService<S> {
    inner: S,
    access_rules: Arc<firewall_base::matcher::LocationRulesMatcher>,
    missing_rule_policy: MissingRulePolicy,
}

impl<S> tower_service::Service<http::Request<ReqBody>> for AccessControlService<S>
where
    S: tower_service::Service<http::Request<ReqBody>, Response = axum::response::Response, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send,
{
    type Response = axum::response::Response;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: http::Request<ReqBody>) -> Self::Future {
        let mut inner = self.inner.clone();
        let access_rules = self.access_rules.clone();
        let missing_rule_policy = self.missing_rule_policy;

        Box::pin(async move {
            let client_name = request
                .extensions()
                .get::<Arc<dyn agent::RemoteAgent>>()
                .map(|a| a.name().to_owned());
            let http_request = firewall_base::expr::atomics::HttpRequest::new(
                client_name.as_deref(),
                &request,
            );

            let action = match access_rules.match_rule(request.uri().path(), &http_request) {
                Ok((_location, action)) => action,
                Err(error) => {
                    warn!(
                        path = %request.uri().path(),
                        ?missing_rule_policy,
                        error = %Report::from_error(&error),
                        "firewall rule matching failed"
                    );
                    match missing_rule_policy {
                        MissingRulePolicy::Allow => firewall_base::action::RequestAction::Allow,
                        MissingRulePolicy::Deny => firewall_base::action::RequestAction::Deny,
                    }
                }
            };

            if action == firewall_base::action::RequestAction::Deny {
                let name = client_name.as_deref().unwrap_or("<anonymous>");
                info!(
                    client_name = name,
                    uri = %request.uri(),
                    "firewall rules denied request"
                );
                return Ok(axum::response::IntoResponse::into_response(
                    StatusCode::FORBIDDEN,
                ));
            }

            inner.call(request).await
        })
    }
}

// ---------------------------------------------------------------------------
// AccessLogLayer
// ---------------------------------------------------------------------------

/// Tower layer that logs every request/response in Combined Log Format.
///
/// Wraps the inner service and records status code after the response is
/// produced. This ensures all code paths (including errors) are logged.
#[derive(Clone)]
pub struct AccessLogLayer;

impl<S> tower::Layer<S> for AccessLogLayer {
    type Service = AccessLogService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AccessLogService { inner }
    }
}

#[derive(Clone)]
pub struct AccessLogService<S> {
    inner: S,
}

impl<S> tower_service::Service<http::Request<ReqBody>> for AccessLogService<S>
where
    S: tower_service::Service<http::Request<ReqBody>, Response = axum::response::Response, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send,
{
    type Response = axum::response::Response;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: http::Request<ReqBody>) -> Self::Future {
        let mut inner = self.inner.clone();
        let req_info = RequestInfo::from_request(&request);

        Box::pin(async move {
            let response = inner.call(request).await?;

            let status = response.status().as_u16();
            // Content-Length gives us the body size without consuming the body
            let body_size = response
                .headers()
                .get(http::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);

            req_info.log_access(status, body_size).await;

            Ok(response)
        })
    }
}
