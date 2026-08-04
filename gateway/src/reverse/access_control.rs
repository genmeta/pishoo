use std::sync::Arc;

use axum::{
    extract::{Request, State},
    middleware::Next,
    response::{IntoResponse, Response},
};
use dhttp::{
    h3x::{connection::ConnectionState, quic},
    identity::RemoteAuthorityCertificateExt as _,
};
use http::StatusCode;
use tracing::{info, warn};

/// Shared state for the access control middleware.
#[derive(Clone)]
pub struct AccessControlState {
    pub acl: Option<Arc<access_control::AccessService>>,
}

pub fn access_control(
    State(state): State<AccessControlState>,
    mut request: Request,
    next: Next,
) -> futures::future::BoxFuture<'static, Response> {
    Box::pin(async move {
        let Some(acl) = state.acl else {
            return next.run(request).await;
        };

        let connection = request
            .extensions()
            .get::<Arc<ConnectionState<dyn quic::DynConnection>>>()
            .cloned();
        let visitor = match remote_visitor(connection).await {
            Ok(visitor) => visitor,
            Err(error) => {
                warn!(error, uri = %request.uri(), "failed to read verified remote identity");
                return StatusCode::FORBIDDEN.into_response();
            }
        };
        let (name, subject_id) = visitor.as_ref().map_or((None, None), |visitor| {
            (Some(visitor.name()), Some(visitor.subject_id()))
        });
        if let Some(visitor) = &visitor {
            request.extensions_mut().insert(visitor.clone());
        }
        let headers = access_control::Headers {
            method: request.method().clone(),
            path: request.uri().path_and_query().map_or_else(
                || request.uri().path().to_owned(),
                |path| path.as_str().to_owned(),
            ),
            fields: request.headers().clone(),
            request_id: None,
        };

        match acl.auth(headers, name, subject_id).await {
            Ok(access_control::AuthResult::Allowed) => next.run(request).await,
            Ok(access_control::AuthResult::Denied) => {
                info!(client_name = name, uri = %request.uri(), "access control denied request");
                StatusCode::FORBIDDEN.into_response()
            }
            Ok(access_control::AuthResult::Reviewing(id, state, reviews)) => {
                let decision = state.await;
                reviews.del(id);
                match decision {
                    Ok(access_control::Action::Allow) => next.run(request).await,
                    Ok(access_control::Action::Deny) => {
                        info!(review_id = id, client_name = name, uri = %request.uri(), "access control review denied request");
                        StatusCode::FORBIDDEN.into_response()
                    }
                    Err(error) => {
                        info!(review_id = id, client_name = name, uri = %request.uri(), %error, "access control review was cancelled");
                        StatusCode::FORBIDDEN.into_response()
                    }
                }
            }
            Err(error) => {
                warn!(error = %error, client_name = name, uri = %request.uri(), "access control evaluation failed");
                StatusCode::FORBIDDEN.into_response()
            }
        }
    })
}

async fn remote_visitor(
    connection: Option<Arc<ConnectionState<dyn quic::DynConnection>>>,
) -> Result<Option<access_control::Visitor>, String> {
    let Some(connection) = connection else {
        return Ok(None);
    };
    let authority = connection
        .remote_authority()
        .await
        .map_err(|error| error.to_string())?;
    let Some(authority) = authority else {
        return Ok(None);
    };
    let subject_key_identifier = authority
        .dhttp_subject_key_identifier()
        .map_err(|error| error.to_string())?;
    let owner_hash = subject_key_identifier.owner_hash().as_str();
    let subject_id = access_control::SubjectId::new(owner_hash.as_bytes())
        .map_err(|_| String::from("invalid DHTTP owner hash length"))?;

    Ok(Some(access_control::Visitor::new(
        authority.name(),
        subject_id,
    )))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{Router, body::Body, middleware::from_fn_with_state, routing::get};
    use http::{Request, StatusCode};
    use tower::ServiceExt;

    use super::AccessControlState;

    #[tokio::test]
    async fn profile_with_an_empty_access_database_denies_requests() {
        let state = AccessControlState {
            acl: Some(Arc::new(
                access_control::AccessService::load_from_db(
                    "sqlite::memory:",
                    "server.example",
                    &access_control::SubjectId::new([1]).unwrap(),
                )
                .await
                .unwrap(),
            )),
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

    #[tokio::test]
    async fn direct_server_without_access_control_allows_requests() {
        let state = AccessControlState { acl: None };
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(from_fn_with_state(state, super::access_control));

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn reviewed_request_waits_for_and_obeys_approval() {
        let service = Arc::new(
            access_control::AccessService::load_from_db(
                "sqlite::memory:",
                "server.example",
                &access_control::SubjectId::new([1]).unwrap(),
            )
            .await
            .unwrap(),
        );
        service
            .set_policy(
                access_control::Method::Unspecified,
                "/",
                access_control::Effect::Review,
                access_control::Grantee::All,
            )
            .await
            .unwrap();
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(from_fn_with_state(
                AccessControlState {
                    acl: Some(service.clone()),
                },
                super::access_control,
            ));
        let request = tokio::spawn(async move {
            app.oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
                .await
                .unwrap()
        });
        let id = loop {
            let (_, reviews) = service.pending_live_reviews(0, 1);
            if let Some(access_control::ReviewRecord::Live { id, .. }) = reviews.into_iter().next()
            {
                break id;
            }
            tokio::task::yield_now().await;
        };
        service
            .decide_review(
                access_control::ReviewTarget::Live(id),
                access_control::Action::Allow,
                None,
            )
            .await
            .unwrap();
        assert_eq!(request.await.unwrap().status(), StatusCode::OK);
    }
}
