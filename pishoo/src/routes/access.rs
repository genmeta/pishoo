use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use access_control::{
    AccessService, Action, AuthResult, ContactNotifier, Headers, NotifyError, SubjectId, Visitor,
};
use axum::{
    Router,
    body::Body as AxumBody,
    response::{IntoResponse, Response},
    routing::any,
};
use bytes::Bytes;
use http::{HeaderValue, Method, Request, StatusCode, Uri, header};
use http_body_util::{BodyExt, Full};

use super::reject;
use crate::Error;

// Pishoo owns neither ContactNotifier nor Endpoint, so the trait needs a local type.
struct DhttpContactNotifier {
    endpoint: dhttp::Endpoint,
}

impl ContactNotifier for DhttpContactNotifier {
    fn submit_application<'a>(
        &'a self,
        contact: &'a str,
        body: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = Result<SubjectId, NotifyError>> + Send + 'a>> {
        Box::pin(async move {
            dhttp_home::validate_name(contact)?;
            let target = dhttp_home::normalize_name(contact)
                .ok_or_else(|| std::io::Error::other("invalid contact target"))?;
            let uri: Uri = format!("https://{target}/contact").parse()?;
            let response = self
                .endpoint
                .post(uri)
                .header(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                )
                .body(Full::new(Bytes::from(body)))
                .await?;
            if response.status() != StatusCode::CREATED {
                return Err(std::io::Error::other(format!(
                    "contact application returned {}",
                    response.status()
                ))
                .into());
            }
            let subject = {
                let peer = response
                    .extensions()
                    .get::<dhttp::RemoteAuthority>()
                    .ok_or_else(|| {
                        std::io::Error::other("contact response has no verified peer")
                    })?;
                if dhttp_home::normalize_name(peer.name()).as_deref() != Some(target.as_str()) {
                    return Err(
                        std::io::Error::other("contact response peer name mismatched").into(),
                    );
                }
                let ski = dhttp_home::certificate::extract_dhttp_subject_key_identifier(
                    peer.certificates(),
                )?;
                SubjectId::new(ski.owner_hash().as_str().as_bytes())
                    .map_err(|_| std::io::Error::other("invalid contact owner hash"))?
            };
            response.into_body().collect().await?;
            Ok(subject)
        })
    }

    fn granted_update<'a>(
        &'a self,
        contact: &'a str,
        modified_since: i64,
        body: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = Result<(), NotifyError>> + Send + 'a>> {
        Box::pin(async move {
            dhttp_home::validate_name(contact)?;
            let uri: Uri = format!("https://{contact}/contact").parse()?;
            let timestamp = chrono::DateTime::<chrono::Utc>::from_timestamp(modified_since, 0)
                .ok_or_else(|| std::io::Error::other("contact timestamp is not representable"))?;
            let date =
                HeaderValue::from_str(&timestamp.format("%a, %d %b %Y %H:%M:%S GMT").to_string())?;

            tokio::time::timeout(Duration::from_secs(30), async {
                let response = self
                    .endpoint
                    .patch(uri)
                    .header(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("application/json"),
                    )
                    .header(header::IF_MODIFIED_SINCE, date)
                    .body(Full::new(Bytes::from(body)))
                    .await?;
                if response.status() != StatusCode::NO_CONTENT {
                    return Err(std::io::Error::other(format!(
                        "contact notification returned {}",
                        response.status()
                    ))
                    .into());
                }
                response.into_body().collect().await?;
                Ok::<(), NotifyError>(())
            })
            .await?
        })
    }
}

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

pub(crate) fn access_router(
    access: Arc<AccessService>,
    endpoint: dhttp::Endpoint,
    profile: &str,
    owner_name: &str,
) -> Router {
    let context = serde_json::json!({ "profile": profile, "owner_name": owner_name, "development_identity": false, "demo_data": false, "version": env!("CARGO_PKG_VERSION") });
    access_control::management_router_with_notifier(
        access,
        Some(Arc::new(DhttpContactNotifier { endpoint })),
    )
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
