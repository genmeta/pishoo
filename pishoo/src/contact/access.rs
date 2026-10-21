//! Owner identity and access to daccess contacts and ACL rules.
use std::sync::Arc;

use access_control::AccessService;
use axum::body::Body;
use http::{Method, Request};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::{
    Failure,
    capabilities::{offered, requested},
    fail,
};

pub(super) fn owner(endpoint: &dhttp::Endpoint) -> Result<access_control::Visitor, Failure> {
    let local = endpoint
        .local_authority()
        .map_err(|e| fail(500, e.to_string()))?;
    let ski = dhttp_home::certificate::extract_dhttp_subject_key_identifier(local.certificates())
        .map_err(|e| fail(500, e.to_string()))?;
    Ok(access_control::Visitor::new(
        endpoint.name(),
        access_control::SubjectId::new(ski.owner_hash().as_str().as_bytes())
            .map_err(|_| fail(500, "invalid local subject identity"))?,
    ))
}
pub(super) async fn local_api(
    access: &Arc<AccessService>,
    endpoint: &dhttp::Endpoint,
    method: Method,
    path: &str,
    body: Option<&Value>,
) -> Result<Value, Failure> {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(
            body.map_or_else(Vec::new, |v| serde_json::to_vec(v).unwrap()),
        ))
        .map_err(|e| fail(500, e.to_string()))?;
    let visitor = owner(endpoint)?;
    let headers = access_control::Headers {
        method: request.method().clone(),
        path: path.to_owned(),
        fields: request.headers().clone(),
    };
    match access
        .auth(headers, Some(visitor.name()), Some(visitor.subject_id()))
        .await
        .map_err(|e| fail(500, e.to_string()))?
    {
        access_control::AuthResult::Allowed => {}
        access_control::AuthResult::Denied => {
            return Err(fail(403, "contact operation denied by ACL"));
        }
        access_control::AuthResult::Reviewing(_) | access_control::AuthResult::Waiting(_) => {
            return Err(fail(409, "contact operation awaits ACL approval"));
        }
    }
    request.extensions_mut().insert(visitor);
    let response = crate::routes::access_router(access.clone())
        .oneshot(request)
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .map_err(|e| fail(502, e.to_string()))?;
    if !(200..300).contains(&status) {
        return Err(fail(status, String::from_utf8_lossy(&bytes).into_owned()));
    }
    if bytes.is_empty() {
        Ok(Value::Null)
    } else {
        serde_json::from_slice(&bytes).map_err(|_| fail(502, "invalid contact response"))
    }
}
pub(super) async fn contact(
    access: &Arc<AccessService>,
    endpoint: &dhttp::Endpoint,
    target: &str,
) -> Result<Value, Failure> {
    local_api(
        access,
        endpoint,
        Method::GET,
        &format!("/std/contact?name={}", encoded(target)),
        None,
    )
    .await
}
pub(super) fn encoded(value: &str) -> String {
    form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

/// Persist only an identity learned through an authenticated peer connection.
pub(super) async fn record_outbound(
    access: &AccessService,
    target: &str,
    subject: &str,
    requests: Value,
    offers: Value,
    valid_before: i64,
    started_at: i64,
) -> Result<(), Failure> {
    let time = super::now();
    let contact = access_control::NewContact {
        name: target.into(),
        subject_id: access_control::SubjectId::new(subject.as_bytes())
            .map_err(|_| fail(502, "invalid peer subject"))?,
        class: "Human".into(),
        description: String::new(),
        requested_access: serde_json::from_value(requests).map_err(|e| fail(400, e.to_string()))?,
        offers: serde_json::from_value(offers).map_err(|e| fail(400, e.to_string()))?,
        created_at: started_at,
        updated_at: time,
        expired_after: valid_before,
    };
    access
        .record_outbound_contact(contact)
        .await
        .map_err(|error| {
            let code = match &error {
                sea_orm::DbErr::RecordNotUpdated => 409,
                sea_orm::DbErr::Type(_) => 400,
                _ => 500,
            };
            fail(code, error.to_string())
        })
}

// Called only by the owner's submission, before enqueueing. Delivery and
// acceptance never replay grants or restore permissions the owner revoked.
pub(super) async fn grant_offered(
    access: &Arc<AccessService>,
    endpoint: &dhttp::Endpoint,
    target: &str,
    subject: &str,
    requests: &[Value],
    capabilities: &[Value],
    valid_before: i64,
) -> Result<(), Failure> {
    let existing = match contact(access, endpoint, target).await {
        Ok(contact) => contact,
        Err((404, _)) => {
            record_outbound(
                access,
                target,
                subject,
                requested(requests),
                offered(capabilities),
                valid_before,
                super::now(),
            )
            .await?;
            contact(access, endpoint, target).await?
        }
        Err(error) => return Err(error),
    };
    if existing["subject_id"] != subject
        || !matches!(existing["status"].as_str(), Some("pending" | "active"))
    {
        return Err(fail(409, "local contact blocks offered capabilities"));
    }
    for capability in capabilities {
        for operation in capability["endpoints"].as_array().unwrap() {
            local_api(
                access,
                endpoint,
                Method::PATCH,
                &format!("/std/acl/allow?name={}", encoded(target)),
                Some(
                    &json!({"api":operation["path"],"method":operation["method"],"effect":"allow"}),
                ),
            )
            .await?;
        }
    }
    Ok(())
}
