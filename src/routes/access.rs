use std::sync::Arc;

use access_control::{
    AccessService, Action, AuthResult, Headers, PendingReviewResponse, SubjectId, Visitor,
};
use axum::{
    Router,
    response::{IntoResponse, Response},
};
use http::StatusCode;

use crate::Error;

// Authorization failures must not outlive the policy decision in a cache.
// Successful guest/proxy responses retain their own cache semantics.
fn reject(error: Error) -> Response {
    (
        [(http::header::CACHE_CONTROL, "no-store")],
        super::reject(error),
    )
        .into_response()
}

pub(crate) async fn authorize(
    axum::extract::State(access): axum::extract::State<Arc<AccessService>>,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if request.uri().path().starts_with("/std/contact")
        && !matches!(*request.method(), http::Method::HEAD | http::Method::GET)
        && request
            .headers()
            .get("sec-fetch-site")
            .is_some_and(|v| v == "cross-site")
    {
        return reject(Error::Denied);
    }
    request.headers_mut().remove("pishoo-source-lib");
    if request.headers().contains_key("pishoo-act-as") {
        return reject(Error::Denied);
    }
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
    // The host supplies the direct Lib only for calls within this identity.
    let standard = |id: &str| {
        id.split_once('/').is_some_and(|(author, name)| {
            crate::package::is_standard_author(author) && name.starts_with("std/")
        })
    };
    let service = request.extensions().get::<Arc<crate::sandbox::Lib>>();
    let probe = request.headers().get("pishoo-check-access").is_some();
    if probe
        && !(request
            .headers()
            .get_all("pishoo-check-access")
            .iter()
            .count()
            == 1
            && request.headers()["pishoo-check-access"] == "1"
            && service.is_some_and(|lib| {
                lib.id.split_once('/').is_some_and(|(author, name)| {
                    crate::package::is_standard_author(author) && name == "std/comm/message"
                })
            })
            && request.uri().path().starts_with("/std/comm/message/"))
    {
        return reject(Error::Denied);
    }
    let standard_read = service.is_some_and(|lib| standard(&lib.id))
        && request.method() == http::Method::GET
        && request.uri().path().starts_with("/std/");
    let service_authorized_provider = service
        .and_then(|_| {
            request
                .extensions()
                .get::<Arc<std::sync::RwLock<crate::sandbox::Sandbox>>>()
        })
        .zip(request.extensions().get::<dhttp::Endpoint>())
        .and_then(|(sandbox, endpoint)| {
            crate::sandbox::select_library(&sandbox.read().unwrap(), endpoint, &request)
                .ok()
                .map(|(lib, _)| lib)
        })
        .filter(|lib| {
            standard_read && standard(&lib.id)
                || lib
                    .openapi
                    .extensions
                    .get("pishoo-service-authorization")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)
        });
    if let Some(provider) = service_authorized_provider.filter(|_| !probe) {
        request
            .extensions_mut()
            .insert(crate::sandbox::AuthorizedProvider(provider));
        return next.run(request).await;
    }
    // Contact authorizes directory writes in its own handler.
    let contact_path = request.uri().path();
    let item = contact_path
        .strip_prefix("/std/contact/")
        .filter(|p| !p.contains('/') && !matches!(*p, "list" | "requests"));
    if service.is_some()
        && ((request.method() == http::Method::GET
            && (contact_path == "/std/contact" || item.is_some()))
            || (request.method() == http::Method::PUT
                && (contact_path == "/std/contact" || item.is_some_and(|p| p.contains("~%23"))))
            || (request.method() == http::Method::DELETE
                && item.is_some_and(|p| p.contains("~%23"))))
    {
        return next.run(request).await;
    }
    // A frontend is one read-only resource. Keep the original URI for routing,
    // while pages, assets and deep links share its root GET authorization.
    let app = matches!(request.method(), &http::Method::GET | &http::Method::HEAD)
        .then(|| request.uri().path().strip_prefix("/app/"))
        .flatten()
        .and_then(|tail| {
            let installed = request
                .extensions()
                .get::<Arc<std::sync::RwLock<crate::sandbox::Sandbox>>>()
                .and_then(|sandbox| {
                    sandbox
                        .read()
                        .unwrap()
                        .apps
                        .keys()
                        .filter_map(|id| {
                            crate::package::app_mount(id, tail).map(|mount| (mount, id))
                        })
                        .max_by_key(|(mount, _)| mount.len())
                        .map(|(_, id)| crate::package::app_name(id).to_owned())
                });
            let local = request
                .extensions()
                .get::<dhttp_home::identity::IdentityProfile>()
                .and_then(|profile| crate::development::preview_mount(profile, tail, true))
                .map(|(name, _)| crate::package::app_name(&name).to_owned());
            installed
                .into_iter()
                .chain(local)
                .max_by_key(String::len)
                .or_else(|| {
                    tail.split('/')
                        .next()
                        .filter(|name| !name.is_empty())
                        .map(str::to_owned)
                })
        })
        .map(|name| format!("/app/{name}"));
    let app_head = app.is_some() && request.method() == http::Method::HEAD;
    let mut compatibility = None;
    let (method, path) = if let Some(app) = app {
        (http::Method::GET, app)
    } else {
        let file_provider = if !probe
            && request.method() == http::Method::POST
            && request.uri().path() == "/std/comm/message"
        {
            request
                .extensions()
                .get::<Arc<std::sync::RwLock<crate::sandbox::Sandbox>>>()
                .zip(request.extensions().get::<dhttp::Endpoint>())
                .and_then(|(sandbox, endpoint)| {
                    crate::sandbox::select_library(&sandbox.read().unwrap(), endpoint, &request)
                        .ok()
                })
                .map(|(lib, _)| lib)
                .filter(|lib| {
                    lib.id.split_once('/').is_some_and(|(author, name)| {
                        crate::package::is_standard_author(author) && name == "std/comm/message"
                    }) && lib
                        .openapi
                        .extensions
                        .get("pishoo-resource-acl")
                        .and_then(serde_json::Value::as_bool)
                        == Some(true)
                })
        } else {
            None
        };
        let (method, path) = match super::resource::operation(
            &mut request,
            visitor.as_ref(),
            file_provider.is_some(),
        )
        .await
        {
            Ok(Some(op)) => op,
            Ok(None) => {
                let Some(provider) = file_provider else {
                    return reject(Error::Denied);
                };
                request
                    .extensions_mut()
                    .insert(crate::sandbox::AuthorizedProvider(provider));
                return next.run(request).await;
            }
            Err(error) => return reject(error),
        };
        let local = request
            .extensions()
            .get::<dhttp::Endpoint>()
            .map(dhttp::Endpoint::name);
        let original = (method, path);
        match super::resource::compatibility_operation(
            &access,
            original.0.clone(),
            original.1.clone(),
            visitor.as_ref(),
            local,
        )
        .await
        {
            Ok(op) => {
                if op != original {
                    compatibility = Some((original, op.clone()));
                }
                op
            }
            Err(error) => return reject(error),
        }
    };
    let stream = if method == http::Method::GET
        && (path == "/std/comm/message/history" || path.starts_with("/std/forward/"))
        && !probe
    {
        match stream_rules(&access, &path).await {
            Ok(snapshot) => Some((path.clone(), snapshot)),
            Err(error) => return reject(Error::InvalidConfig(error.to_string())),
        }
    } else {
        None
    };
    let headers = Headers {
        method,
        path,
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
            let mut response = (StatusCode::ACCEPTED, axum::Json(body)).into_response();
            response.headers_mut().insert(
                http::header::CACHE_CONTROL,
                http::HeaderValue::from_static("no-store"),
            );
            response
                .headers_mut()
                .insert(http::header::LOCATION, location.parse().unwrap());
            response
                .headers_mut()
                .insert("pishoo-review-hops", http::HeaderValue::from_static("0"));
            response.headers_mut().insert(
                http::header::RETRY_AFTER,
                http::HeaderValue::from_static("3"),
            );
            return response;
        }
        Ok(AuthResult::Waiting(review)) => {
            let (status, error) =
                match tokio::time::timeout(std::time::Duration::from_secs(300), review).await {
                    Ok(Ok(Action::Allow)) => (StatusCode::OK, ""),
                    Ok(Ok(Action::Deny)) => (StatusCode::FORBIDDEN, "review_denied"),
                    Ok(Err(_)) => (StatusCode::SERVICE_UNAVAILABLE, "review_unavailable"),
                    Err(_) => (StatusCode::GATEWAY_TIMEOUT, "review_timeout"),
                };
            if status != StatusCode::OK {
                return (
                    status,
                    [(http::header::CACHE_CONTROL, "no-store")],
                    axum::Json(serde_json::json!({"error": error})),
                )
                    .into_response();
            }
            true
        }
        Err(sea_orm::DbErr::Type(message)) => {
            return (
                StatusCode::BAD_REQUEST,
                [(http::header::CACHE_CONTROL, "no-store")],
                message,
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "authorization failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(http::header::CACHE_CONTROL, "no-store")],
            )
                .into_response();
        }
    };
    if !allowed {
        return reject(Error::Denied);
    }
    // A concrete rule may have been installed while an older permission was
    // awaiting review. Recheck the mapping before dispatching the action.
    if let Some(((method, path), selected)) = compatibility {
        let local = request
            .extensions()
            .get::<dhttp::Endpoint>()
            .map(dhttp::Endpoint::name);
        match super::resource::compatibility_operation(
            &access,
            method,
            path,
            visitor.as_ref(),
            local,
        )
        .await
        {
            Ok(current) if current == selected => (),
            Ok(_) => {
                return (
                    StatusCode::CONFLICT,
                    [(http::header::CACHE_CONTROL, "no-store")],
                    "communication policy changed; retry",
                )
                    .into_response();
            }
            Err(error) => return reject(error),
        }
    }
    if probe {
        return (
            StatusCode::NO_CONTENT,
            [(http::header::CACHE_CONTROL, "no-store")],
        )
            .into_response();
    }
    if let Err(error) = super::resource::rule_target(&mut request).await {
        return reject(error);
    }
    let owner_subject = if app_head {
        let authority = request
            .extensions()
            .get::<dhttp::HandshakeSummary>()
            .and_then(|handshake| handshake.local.as_ref())
            .or_else(|| request.extensions().get::<dhttp::LocalAuthority>());
        authority.zip(visitor.as_ref()).and_then(|(local, caller)| {
            let ski =
                dhttp_home::certificate::extract_dhttp_subject_key_identifier(local.certificates())
                    .ok()?;
            (caller.name() == local.name()
                && caller.subject_id().as_bytes() == ski.owner_hash().as_str().as_bytes())
            .then(|| http::HeaderValue::from_str(ski.owner_hash().as_str()).ok())
            .flatten()
        })
    } else {
        None
    };
    let mut response = next.run(request).await;
    if let Some((path, snapshot)) = stream
        && response.status().is_success()
        && response
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(super::event_stream_content_type)
    {
        use http_body_util::BodyExt;
        let (parts, mut body) = response.into_parts();
        let body = axum::body::Body::from_stream(async_stream::stream! {
            while let Some(frame)=body.frame().await {
                if !stream_rules(&access,&path).await.is_ok_and(|current|current==snapshot) {break;}
                match frame {
                    Ok(frame)=>if let Ok(data)=frame.into_data() {yield Ok(data);},
                    Err(error)=>{yield Err(error);break;}
                }
            }
        });
        response = Response::from_parts(parts, body);
    }
    if app_head {
        // Let an App read its current transport identity through its existing
        // page request. Never cache identity-dependent response metadata.
        let name = visitor.as_ref().map_or("", Visitor::name);
        let Ok(name) = http::HeaderValue::from_str(name) else {
            return reject(Error::Denied);
        };
        response.headers_mut().insert("pishoo-viewer-name", name);
        response.headers_mut().remove("pishoo-owner-subject");
        if let Some(subject) = owner_subject {
            response
                .headers_mut()
                .insert("pishoo-owner-subject", subject);
        }
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

// A live event stream must stop before delivering the next frame when the
// relevant policy changes. Reconnection uses the normal ACL and saved cursor.
async fn stream_rules(
    access: &AccessService,
    path: &str,
) -> Result<Vec<(String, String, String)>, sea_orm::DbErr> {
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
    let rows=access.database().query_all_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT api,grantee,method||':'||effect AS decision FROM access_rules WHERE method IN ('GET','*') AND (api='/' OR api=?1 OR instr(?1,api||'/')=1) ORDER BY api,grantee,method",
        [path.into()])).await?;
    rows.into_iter()
        .map(|r| {
            Ok((
                r.try_get("", "api")?,
                r.try_get("", "grantee")?,
                r.try_get("", "decision")?,
            ))
        })
        .collect()
}
