use std::sync::Arc;

use access_control::{
    AccessService, AuthResult, Headers, PendingReviewResponse, SubjectId, Visitor,
};
use axum::{
    Router,
    body::Body as AxumBody,
    response::{IntoResponse, Response},
    routing::any,
};
use http::{Method, Request, StatusCode, header};
use include_dir::{Dir, include_dir};

use super::reject;
use crate::Error;

static WORKSPACE_DIST: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/workspace/dist");

pub(crate) async fn authorize(
    axum::extract::State(access): axum::extract::State<Arc<AccessService>>,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    request.extensions_mut().remove::<Visitor>();
    let Some(handshake) = request.extensions().get::<dhttp::HandshakeSummary>() else {
        return reject(Error::MissingHandshake);
    };
    if handshake.local.is_none() {
        return reject(Error::MissingHandshake);
    }
    let visitor = match &handshake.remote {
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
    };
    if let Some(visitor) = &visitor {
        request.extensions_mut().insert(visitor.clone());
    }
    if access_control::is_review_status_path(request.method(), request.uri().path())
        || access_control::is_contact_status_path(request.method(), request.uri().path())
        || crate::workspace::capabilities::BuiltInCapability::public_endpoint(
            request.method(),
            request.uri().path(),
        )
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
        .route(
            "/workspace",
            any(|| async {
                (
                    StatusCode::TEMPORARY_REDIRECT,
                    [(header::LOCATION, "/workspace/")],
                )
            }),
        )
        .route("/workspace/", any(workspace))
        .route("/workspace/{*path}", any(workspace))
}

pub(super) async fn workspace(request: Request<AxumBody>) -> Response {
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return reject(Error::MethodNotAllowed);
    }
    let path = request
        .uri()
        .path()
        .strip_prefix("/workspace/")
        .unwrap_or_default();
    if path.split('/').any(|p| p == "..") {
        return reject(Error::RouteNotFound);
    }
    let file = WORKSPACE_DIST.get_file(path).or_else(|| {
        std::path::Path::new(path)
            .extension()
            .is_none()
            .then(|| WORKSPACE_DIST.get_file("index.html"))
            .flatten()
    });
    let Some(file) = file else {
        return reject(Error::RouteNotFound);
    };
    let content_type = mime_guess::from_path(file.path()).first_or_octet_stream();
    let cache = if file.path().starts_with("assets") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let mut response = (
        [
            (header::CONTENT_TYPE, content_type.as_ref()),
            (header::CACHE_CONTROL, cache),
        ],
        file.contents(),
    )
        .into_response();
    if request.method() == Method::HEAD {
        *response.body_mut() = AxumBody::empty();
    }
    response
}
