use std::sync::Arc;

use axum::{
    extract::{Request, State},
    middleware::Next,
    response::{IntoResponse, Response},
};
use dhttp::{
    access::{
        expr::{
            atomics::{AtomicLocationRuleExpr, EvalError},
            eval::Evaluable,
        },
        matcher::{LocationRulesMatcher, MatchRuleFailed},
        pattern::NormalPattern,
    },
    h3x::{connection::ConnectionState, quic},
};
use http::{HeaderMap, HeaderValue, Method, StatusCode};
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

impl Evaluable<AccessHttpRequest<'_>> for AtomicLocationRuleExpr {
    type Value = Result<bool, EvalError>;

    fn eval(&self, request: &AccessHttpRequest<'_>) -> Self::Value {
        Ok(match self {
            Self::Any(..) => true,
            Self::ClientName(pattern) => pattern.eval(&request.client_name)?,
            Self::Method(method) => {
                let pattern: &NormalPattern = method.as_ref();
                pattern.eval(&request.method.as_str())
            }
            Self::Header(header) => request.headers.iter().any(|(key, value)| {
                let Ok(value) = value.to_str() else {
                    return false;
                };
                header.eval(&(key.as_str(), value))
            }),
            Self::Query(query) => request.queries.iter().any(|pair| query.eval(pair)),
        })
    }
}

/// Shared state for the access control middleware.
#[derive(Clone)]
pub struct AccessControlState {
    pub access_rules: Arc<LocationRulesMatcher>,
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
                dhttp::access::action::RequestAction::Allow
            } else {
                warn!(
                    path = %request.uri().path(),
                    client = client_name.as_deref(),
                    "no ruleset matched, denying non-self client"
                );
                dhttp::access::action::RequestAction::Deny
            }
        }
        Err(MatchRuleFailed::MatchRuleInSet) => {
            // Ruleset matched but no rule matched — deny everyone.
            warn!(
                path = %request.uri().path(),
                "ruleset matched but no rule matched, denying all"
            );
            dhttp::access::action::RequestAction::Deny
        }
    };

    if action == dhttp::access::action::RequestAction::Deny {
        info!(
            client_name = client_name.as_deref(),
            uri = %request.uri(),
            "firewall rules denied request"
        );
        return StatusCode::FORBIDDEN.into_response();
    }

    next.run(request).await
}
