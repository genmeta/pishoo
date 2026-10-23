use std::sync::Arc;

use access_control::{
    AccessService, Action, AuthResult, Headers, PendingReviewResponse, SubjectId, Visitor,
};
use axum::{
    Router,
    response::{IntoResponse, Response},
};
use http::StatusCode;

use super::reject;
use crate::Error;

pub(crate) async fn authorize(
    axum::extract::State(access): axum::extract::State<Arc<AccessService>>,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    request.extensions_mut().remove::<Visitor>();
    let visitor = if let Some(handshake) = request.extensions().get::<dhttp::HandshakeSummary>() {
        if handshake.local.is_none() {
            return reject(Error::MissingHandshake);
        }
        match &handshake.remote {
            Some(remote) => match dhttp_home::certificate::extract_dhttp_subject_key_identifier(
                remote.certificates(),
            )
            .ok()
            .and_then(|ski| SubjectId::new(ski.owner_hash().as_str().as_bytes()).ok())
            {
                Some(subject) => Some(Visitor::new(remote.name(), subject)),
                None => return reject(Error::Denied),
            },
            None => None,
        }
    } else if let Some(local) = request.extensions().get::<dhttp::LocalAuthority>() {
        match dhttp_home::certificate::extract_dhttp_subject_key_identifier(local.certificates())
            .ok()
            .and_then(|ski| SubjectId::new(ski.owner_hash().as_str().as_bytes()).ok())
        {
            Some(subject) => Some(Visitor::new(local.name(), subject)),
            None => return reject(Error::Denied),
        }
    } else {
        return reject(Error::MissingHandshake);
    };
    if let Some(visitor) = &visitor {
        request.extensions_mut().insert(visitor.clone());
    }
    // A frontend is one read-only resource. Keep the original URI for routing,
    // while pages, assets and deep links share its root GET authorization.
    let app = matches!(request.method(), &http::Method::GET | &http::Method::HEAD)
        .then(|| request.uri().path().strip_prefix("/app/"))
        .flatten()
        .and_then(|tail| tail.split('/').next())
        .filter(|name| !name.is_empty())
        .map(|name| format!("/app/{name}"));
    let app_head = app.is_some() && request.method() == http::Method::HEAD;
    let headers = Headers {
        method: if app.is_some() {
            http::Method::GET
        } else {
            request.method().clone()
        },
        path: app.unwrap_or_else(|| {
            request
                .uri()
                .path_and_query()
                .map_or("/", |p| p.as_str())
                .to_owned()
        }),
        fields: request.headers().clone(),
    };
    let allowed = match access
        .auth(
            headers,
            visitor.as_ref().map(Visitor::name),
            visitor.as_ref().map(Visitor::subject_id),
        )
        .await
    {
        Ok(AuthResult::Allowed) => true,
        Ok(AuthResult::Denied) => false,
        Ok(AuthResult::Reviewing(review)) => {
            // Project the wire contract independently of the pinned daccess
            // DTO, so both old and updated DTO releases produce the same reply.
            let location = format!("/std/acl/status?id={}", review.id());
            let mut body = serde_json::to_value(PendingReviewResponse::from(review))
                .expect("review response contains only serializable values");
            body.as_object_mut().unwrap().remove("error");
            body["status"] = "pending_review".into();
            let mut response = (
                StatusCode::ACCEPTED,
                axum::Json(body),
            )
                .into_response();
            response.headers_mut().insert(
                http::header::CACHE_CONTROL,
                http::HeaderValue::from_static("no-store"),
            );
            response.headers_mut().insert(http::header::LOCATION, location.parse().unwrap());
            response.headers_mut().insert(http::header::RETRY_AFTER, http::HeaderValue::from_static("3"));
            return response;
        }
        Ok(AuthResult::Waiting(review)) => {
            let (status, error) = match tokio::time::timeout(std::time::Duration::from_secs(300), review).await {
                Ok(Ok(Action::Allow)) => (StatusCode::OK, ""),
                Ok(Ok(Action::Deny)) => (StatusCode::FORBIDDEN, "review_denied"),
                Ok(Err(_)) => (StatusCode::SERVICE_UNAVAILABLE, "review_unavailable"),
                Err(_) => (StatusCode::GATEWAY_TIMEOUT, "review_timeout"),
            };
            if status != StatusCode::OK {
                return (status, [(http::header::CACHE_CONTROL, "no-store")],
                    axum::Json(serde_json::json!({"error": error}))).into_response();
            }
            true
        }
        Err(sea_orm::DbErr::Type(message)) => {
            return (StatusCode::BAD_REQUEST, message).into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "authorization failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    if !allowed {
        return reject(Error::Denied);
    }
    let mut response = next.run(request).await;
    if app_head {
        // Let an App read its current transport identity through its existing
        // page request. Never cache identity-dependent response metadata.
        let name = visitor.as_ref().map_or("", Visitor::name);
        let Ok(name) = http::HeaderValue::from_str(name) else {
            return reject(Error::Denied);
        };
        response.headers_mut().insert("pishoo-viewer-name", name);
        response.headers_mut().insert(
            http::header::CACHE_CONTROL,
            http::HeaderValue::from_static("no-store"),
        );
    }
    response
}

pub(crate) fn access_router(access: Arc<AccessService>) -> Router {
    access_control::management_router(access)
}
