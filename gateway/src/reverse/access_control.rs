use std::sync::Arc;

use axum::{
    extract::{Request, State},
    middleware::Next,
    response::{IntoResponse, Response},
};
use dhttp::{
    access::{
        action::RequestAction,
        expr::{
            atomics::{AtomicLocationRuleExpr, EvalError},
            eval::Evaluable,
        },
        pattern::NormalPattern,
        policy::{LocationRuleDecisionError, LocationRuleEvaluator, LocationRuleRequest},
    },
    h3x::{connection::ConnectionState, quic},
};
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use snafu::Report;
use tracing::{info, warn};

struct AccessHttpRequest<'a> {
    client_name: Option<&'a str>,
    method: &'a Method,
    headers: &'a HeaderMap<HeaderValue>,
    queries: Vec<(&'a str, &'a str)>,
}

impl<'a> AccessHttpRequest<'a> {
    fn new<T>(client_name: Option<&'a str>, request: &'a http::Request<T>) -> Self {
        Self {
            client_name,
            method: request.method(),
            headers: request.headers(),
            queries: request.uri().query().map_or_else(Vec::new, |query| {
                query
                    .split('&')
                    .filter_map(|pair| {
                        let mut parts = pair.splitn(2, '=');
                        let key = parts.next()?;
                        let value = parts.next().unwrap_or("");
                        Some((key, value))
                    })
                    .collect()
            }),
        }
    }
}

impl LocationRuleRequest for AccessHttpRequest<'_> {
    fn eval_atomic(&self, expr: &AtomicLocationRuleExpr) -> Result<bool, EvalError> {
        Ok(match expr {
            AtomicLocationRuleExpr::Any(..) => true,
            AtomicLocationRuleExpr::ClientName(pattern) => pattern.eval(&self.client_name)?,
            AtomicLocationRuleExpr::Method(method) => {
                let pattern: &NormalPattern = method.as_ref();
                pattern.eval(&self.method.as_str())
            }
            AtomicLocationRuleExpr::Header(header) => self.headers.iter().any(|(key, value)| {
                let Ok(value) = value.to_str() else {
                    return false;
                };
                header.eval(&(key.as_str(), value))
            }),
            AtomicLocationRuleExpr::Query(query) => {
                self.queries.iter().any(|pair| query.eval(pair))
            }
        })
    }
}

/// Shared state for the access control middleware.
#[derive(Clone)]
pub struct AccessControlState {
    pub access_rules: Arc<dyn LocationRuleEvaluator + Send + Sync>,
    pub server_name: Arc<str>,
}

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
    let client_name = match request
        .extensions()
        .get::<Arc<ConnectionState<dyn quic::DynConnection>>>()
    {
        Some(conn) => match conn.remote_authority().await {
            Ok(Some(authority)) => Some(authority.name().to_owned()),
            Ok(None) => None,
            Err(error) => {
                warn!(error = %error, "failed to fetch remote authority from connection");
                None
            }
        },
        None => None,
    };
    let http_request = AccessHttpRequest::new(client_name.as_deref(), &request);

    let action = match state
        .access_rules
        .evaluate(request.uri().path(), &http_request)
        .await
    {
        Ok(decision) => decision.action,
        Err(LocationRuleDecisionError::NoRuleSet { .. }) => {
            // No ruleset matched the path — allow the server itself, deny others.
            if client_name.as_deref() == Some(&*state.server_name) {
                warn!(
                    path = %request.uri().path(),
                    "no ruleset matched, allowing self only"
                );
                RequestAction::Allow
            } else {
                warn!(
                    path = %request.uri().path(),
                    client = client_name.as_deref(),
                    "no ruleset matched, denying non-self client"
                );
                RequestAction::Deny
            }
        }
        Err(LocationRuleDecisionError::NoRuleInSet { .. }) => {
            // Ruleset matched but no rule matched — deny everyone.
            warn!(
                path = %request.uri().path(),
                "ruleset matched but no rule matched, denying all"
            );
            RequestAction::Deny
        }
        Err(error @ LocationRuleDecisionError::Backend { .. }) => {
            warn!(
                path = %request.uri().path(),
                error = %Report::from_error(&error),
                "failed to evaluate access rules, denying request"
            );
            RequestAction::Deny
        }
    };

    if action == RequestAction::Deny {
        info!(
            client_name = client_name.as_deref(),
            uri = %request.uri(),
            "firewall rules denied request"
        );
        return StatusCode::FORBIDDEN.into_response();
    }

    next.run(request).await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{Router, body::Body, middleware::from_fn_with_state, routing::get};
    use dhttp::access::{
        expr::atomics::AtomicLocationRuleExpr,
        policy::{
            LocationRuleDecisionError, LocationRuleEvaluator, LocationRuleFuture,
            LocationRuleRequest,
        },
    };
    use http::{Request, StatusCode};
    use tower::ServiceExt;

    use super::AccessControlState;

    #[derive(Debug, snafu::Snafu)]
    #[snafu(display("synthetic evaluator failure"))]
    struct SyntheticEvaluatorError;

    struct FailingEvaluator;

    impl LocationRuleEvaluator for FailingEvaluator {
        fn evaluate<'a>(
            &'a self,
            _path: &'a str,
            _request: &'a (dyn LocationRuleRequest + Send + Sync),
        ) -> LocationRuleFuture<'a> {
            Box::pin(
                async move { Err(LocationRuleDecisionError::backend(SyntheticEvaluatorError)) },
            )
        }
    }

    #[tokio::test]
    async fn backend_error_fails_closed_with_forbidden() {
        let state = AccessControlState {
            access_rules: Arc::new(FailingEvaluator),
            server_name: Arc::from("server.pilot.dhttp.net"),
        };
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(from_fn_with_state(state, super::access_control));

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn access_http_request_evaluates_any_client() {
        let request = Request::builder().uri("/").body(()).unwrap();
        let access_request = super::AccessHttpRequest::new(None, &request);
        let expr: AtomicLocationRuleExpr = "*?".parse().unwrap();

        assert!(access_request.eval_atomic(&expr).unwrap());
    }
}
