use std::sync::Arc;

use access_control::{
    AccessService, AuthResult, Headers, PendingReviewResponse, SubjectId, Visitor,
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
    if access_control::is_review_status_path(request.method(), request.uri().path())
        || access_control::is_contact_status_path(request.method(), request.uri().path())
    {
        return next.run(request).await;
    }
    let headers = Headers {
        method: request.method().clone(),
        path: request
            .uri()
            .path_and_query()
            .map_or("/", |p| p.as_str())
            .to_string(),
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
            return (
                StatusCode::ACCEPTED,
                axum::Json(PendingReviewResponse::from(review)),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "authorization failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    if !allowed {
        return reject(Error::Denied);
    }
    next.run(request).await
}

pub(crate) fn access_router(access: Arc<AccessService>) -> Router {
    access_control::management_router(access)
}
