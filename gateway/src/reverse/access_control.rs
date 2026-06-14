use std::sync::Arc;

use axum::{
    extract::{Request, State},
    middleware::Next,
    response::{IntoResponse, Response},
};
use dhttp::{
    access::matcher::{LocationRulesMatcher, MatchRuleFailed},
    h3x::{connection::ConnectionState, quic},
};
use http::StatusCode;
use tracing::{info, warn};

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
    let http_request =
        dhttp::access::expr::atomics::HttpRequest::new(client_name.as_deref(), &request);

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
