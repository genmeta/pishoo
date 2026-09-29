use std::sync::Arc;

use access_control::{AccessService, Action, AuthResult, Headers, SubjectId, Visitor};
use axum::{
    Router,
    body::Body as AxumBody,
    response::{IntoResponse, Response},
    routing::any,
};
use http::{Method, Request, StatusCode, header};

use super::reject;
use crate::Error;

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
    let headers = Headers {
        method: request.method().clone(),
        path: request
            .uri()
            .path_and_query()
            .map_or("/", |p| p.as_str())
            .to_string(),
        fields: request.headers().clone(),
        request_id: None,
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
        Ok(AuthResult::Reviewing(id, state, registry)) => {
            let _cleanup = scopeguard::guard((state.clone(), registry), |(state, registry)| {
                state.cancel();
                registry.del(id);
            });
            matches!(state.await, Ok(Action::Allow))
        }
        Err(e) => {
            eprintln!("authorization failed: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    if !allowed {
        return reject(Error::Denied);
    }
    if let Some(visitor) = visitor {
        request.extensions_mut().insert(visitor);
    }
    next.run(request).await
}

pub(crate) fn worksapce(access: Arc<AccessService>, profile: &str, owner_name: &str) -> Router {
    let context = serde_json::json!({ "profile": profile, "owner_name": owner_name, "development_identity": false, "demo_data": false, "version": env!("CARGO_PKG_VERSION") });
    access_control::management_router(access)
        .route(
            "/workspace-api/context",
            axum::routing::get(move || {
                let context = context.clone();
                async move { axum::Json(context) }
            }),
        )
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
    if path.split('/').any(|p| p == "..") || std::path::Path::new(path).extension().is_some() {
        return reject(Error::RouteNotFound);
    }
    let html = include_str!("../../assets/workspace.html");
    let mut response = (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        html,
    )
        .into_response();
    if request.method() == Method::HEAD {
        *response.body_mut() = AxumBody::empty();
    }
    response
}
