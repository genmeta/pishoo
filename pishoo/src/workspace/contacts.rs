use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use access_control::{
    ContactPatch, ContactStatus, GrantedAccess, Grantee, NewContact, RequestedAccess, SubjectId,
    Visitor,
};
use axum::{
    Extension, Json,
    extract::{Path, Query, State},
};
use http::{Method, StatusCode};
use sea_orm::{ConnectionTrait, DatabaseBackend, DbErr, Statement, TransactionTrait};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    Workspace,
    capabilities::{offered_access_for, requested_access_for},
    settings::require_owner,
};
use crate::chat::CHAT_CAPABILITY;

pub(super) mod worker;

type ApiError = (StatusCode, &'static str);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewRequest {
    target_name: String,
    description: String,
    #[serde(default)]
    requested_capabilities: Vec<String>,
    #[serde(default)]
    offered_capabilities: Vec<String>,
}

#[derive(Serialize)]
pub(crate) struct OutboundRequest {
    id: i64,
    target_name: String,
    description: String,
    requested_capabilities: Vec<String>,
    offered_capabilities: Vec<String>,
    status: String,
    expired_after: i64,
    delivery_deadline: i64,
    remote_expired_after: Option<i64>,
    last_checked_at: Option<i64>,
    error_message: Option<String>,
    created_at: i64,
    updated_at: i64,
}

#[derive(Deserialize)]
#[serde(default)]
pub(crate) struct PageQuery {
    page: u64,
    page_size: Option<u64>,
}

impl Default for PageQuery {
    fn default() -> Self {
        Self {
            page: 1,
            page_size: None,
        }
    }
}

#[derive(Serialize)]
pub(crate) struct RequestPage {
    items: Vec<OutboundRequest>,
    total: u64,
    page: u64,
    page_size: u64,
}

#[derive(Serialize)]
pub(crate) struct CapabilityRequest {
    pub(super) request_id: i64,
    pub(super) contact_name: String,
    pub(super) capability_id: &'static str,
    pub(super) capability_version: &'static str,
    pub(super) status: &'static str,
    pub(super) requested_at: i64,
    pub(super) expired_after: i64,
}

#[derive(Deserialize)]
pub(crate) struct CapabilityDecisionQuery {
    request_id: i64,
    capability_version: String,
}

#[derive(Default, Deserialize)]
pub(crate) struct CapabilityGrantQuery {
    request_id: Option<i64>,
    capability_version: Option<String>,
}

#[derive(Serialize)]
struct RemoteApplication<'a> {
    application_id: &'a str,
    class: &'static str,
    description: &'a str,
    requested_access: &'a RequestedAccess,
    offers: &'a GrantedAccess,
}

#[derive(Deserialize)]
struct RemoteStatus {
    application_id: String,
    status: String,
    name: String,
    subject_id: String,
    received_at: i64,
    expired_after: i64,
    #[serde(default)]
    granted_access: GrantedAccess,
}

fn storage_error(error: DbErr) -> ApiError {
    tracing::error!(error = %error, "Workspace outbound request storage failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "Workspace storage failed",
    )
}

fn now() -> Result<i64, ApiError> {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "invalid system clock"))?
            .as_secs(),
    )
    .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "invalid system clock"))
}

fn selected_access(
    requested_capabilities: &[String],
    offered_capabilities: &[String],
) -> Result<(RequestedAccess, GrantedAccess), ApiError> {
    let mut requested = RequestedAccess::new();
    let mut offers = GrantedAccess::new();
    for capability in requested_capabilities {
        let Some(access) = requested_access_for(capability) else {
            return Err((StatusCode::BAD_REQUEST, "unknown capability"));
        };
        if requested.contains_key("/std/message") {
            return Err((StatusCode::BAD_REQUEST, "duplicate capability"));
        }
        requested.extend(access);
    }
    for capability in offered_capabilities {
        let Some(access) = offered_access_for(capability) else {
            return Err((StatusCode::BAD_REQUEST, "unknown capability"));
        };
        if offers.contains_key("/std/message") {
            return Err((StatusCode::BAD_REQUEST, "duplicate capability"));
        }
        offers.extend(access);
    }
    Ok((requested, offers))
}

fn has_chat_access(requested: &RequestedAccess) -> bool {
    requested.get("/std/message").is_some_and(|methods| {
        methods.as_slice() == [access_control::Method::Specified(Method::POST)]
    })
}

fn access_error(error: DbErr) -> ApiError {
    tracing::error!(error = %error, "Workspace Chat capability update failed");
    let status = match error {
        DbErr::RecordNotFound(_) => StatusCode::NOT_FOUND,
        DbErr::RecordNotUpdated | DbErr::Type(_) => StatusCode::CONFLICT,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, "Chat capability update failed")
}

async fn capability_decision(
    state: &Workspace,
    name: &str,
    capability: &str,
) -> Result<Option<(String, Vec<u8>, i64)>, ApiError> {
    let row = state
        .store
        .db()
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT decision, subject_id, request_id FROM capability_decisions WHERE contact_name = ? AND capability_id = ?",
            [name.into(), capability.into()],
        ))
        .await
        .map_err(storage_error)?;
    row.map(|row| {
        Ok((
            row.try_get("", "decision").map_err(storage_error)?,
            row.try_get("", "subject_id").map_err(storage_error)?,
            row.try_get("", "request_id").map_err(storage_error)?,
        ))
    })
    .transpose()
}

async fn record_capability_decision(
    state: &Workspace,
    name: &str,
    capability: &str,
    decision: &str,
    subject_id: &SubjectId,
    request_id: i64,
) -> Result<(), ApiError> {
    let updated_at = now()?;
    let transaction = state.store.db().begin().await.map_err(storage_error)?;
    let current = transaction
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT decision, descriptor_version, subject_id, request_id FROM capability_decisions WHERE contact_name = ? AND capability_id = ?",
            [name.into(), capability.into()],
        ))
        .await
        .map_err(storage_error)?;
    if let Some(current) = current {
        let current_decision: String = current.try_get("", "decision").map_err(storage_error)?;
        let current_version: String = current
            .try_get("", "descriptor_version")
            .map_err(storage_error)?;
        let current_subject: Vec<u8> = current.try_get("", "subject_id").map_err(storage_error)?;
        let current_request_id: i64 = current.try_get("", "request_id").map_err(storage_error)?;
        if current_decision == decision
            && current_version == CHAT_CAPABILITY.version()
            && current_subject.as_slice() == subject_id.as_bytes()
            && current_request_id == request_id
        {
            transaction.commit().await.map_err(storage_error)?;
            return Ok(());
        }
    }
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "INSERT INTO capability_decision_events (contact_name, capability_id, decision, descriptor_version, subject_id, request_id, decided_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
            [
                name.into(),
                capability.into(),
                decision.into(),
                CHAT_CAPABILITY.version().into(),
                subject_id.as_bytes().to_vec().into(),
                request_id.into(),
                updated_at.into(),
            ],
        ))
        .await
        .map_err(storage_error)?;
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "INSERT INTO capability_decisions (contact_name, capability_id, decision, descriptor_version, subject_id, request_id, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(contact_name, capability_id) DO UPDATE SET decision = excluded.decision, descriptor_version = excluded.descriptor_version, subject_id = excluded.subject_id, request_id = excluded.request_id, updated_at = excluded.updated_at",
            [
                name.into(),
                capability.into(),
                decision.into(),
                CHAT_CAPABILITY.version().into(),
                subject_id.as_bytes().to_vec().into(),
                request_id.into(),
                updated_at.into(),
            ],
        ))
        .await
        .map_err(storage_error)?;
    transaction.commit().await.map_err(storage_error)?;
    Ok(())
}

async fn contact_record_id(
    state: &Workspace,
    name: &str,
    subject_id: &SubjectId,
) -> Result<i64, ApiError> {
    let row = state
        .access
        .database()
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT id FROM contacts WHERE name = ? AND subject_id = ?",
            [name.into(), subject_id.as_bytes().to_vec().into()],
        ))
        .await
        .map_err(access_error)?
        .ok_or((StatusCode::CONFLICT, "contact identity changed"))?;
    row.try_get("", "id").map_err(access_error)
}

pub(crate) async fn ensure_chat_access(state: &Workspace, name: &str) -> Result<(), ApiError> {
    let grantee = Grantee::One(name.to_owned());
    for (method, api, effect) in CHAT_CAPABILITY.fixed_rules() {
        state
            .access
            .set_policy(method, api, effect, grantee.clone())
            .await
            .map_err(access_error)?;
    }
    Ok(())
}

pub(crate) async fn grant_capability(
    state: &Workspace,
    name: &str,
    capability: &str,
    expected: Option<(i64, &str)>,
) -> Result<(), ApiError> {
    if capability != CHAT_CAPABILITY.id() {
        return Err((StatusCode::BAD_REQUEST, "unknown capability"));
    }
    let contact = state
        .access
        .find_contact_by_name(name)
        .await
        .map_err(access_error)?;
    if !has_chat_access(&contact.requested_access) {
        return Err((
            StatusCode::CONFLICT,
            "capability was not requested by this contact",
        ));
    }
    let request_id = contact_record_id(state, name, &contact.subject_id).await?;
    if let Some((expected_id, expected_version)) = expected
        && (expected_id != request_id || expected_version != CHAT_CAPABILITY.version())
    {
        return Err((StatusCode::CONFLICT, "capability request changed"));
    }
    let status = ContactStatus::try_from(contact.status)
        .map_err(|_| (StatusCode::CONFLICT, "contact has invalid status"))?;
    match status {
        ContactStatus::Pending | ContactStatus::Transfered => {
            state
                .access
                .patch_contact(
                    name,
                    ContactPatch::LocalUpdate {
                        status: Some(ContactStatus::Active),
                        alias: None,
                    },
                )
                .await
                .map_err(access_error)?;
        }
        ContactStatus::Active => {}
        ContactStatus::Expired | ContactStatus::Blocked => {
            return Err((
                StatusCode::CONFLICT,
                "contact status cannot grant this capability",
            ));
        }
    }
    ensure_chat_access(state, name).await?;
    record_capability_decision(
        state,
        name,
        CHAT_CAPABILITY.id(),
        "approved",
        &contact.subject_id,
        request_id,
    )
    .await?;
    state.wake_worker();
    Ok(())
}

pub(crate) async fn revoke_capability(
    state: &Workspace,
    name: &str,
    capability: &str,
) -> Result<(), ApiError> {
    if capability != CHAT_CAPABILITY.id() {
        return Err((StatusCode::BAD_REQUEST, "unknown capability"));
    }
    let contact = state
        .access
        .find_contact_by_name(name)
        .await
        .map_err(access_error)?;
    state
        .access
        .remove_policy(
            access_control::Method::Specified(http::Method::POST),
            "/std/message",
            Grantee::One(name.to_owned()),
        )
        .await
        .map_err(access_error)?;
    record_capability_decision(
        state,
        name,
        CHAT_CAPABILITY.id(),
        "revoked",
        &contact.subject_id,
        contact_record_id(state, name, &contact.subject_id).await?,
    )
    .await?;
    Ok(())
}

pub(crate) async fn deny_capability(
    state: &Workspace,
    name: &str,
    capability: &str,
    expected_request_id: i64,
) -> Result<(), ApiError> {
    if capability != CHAT_CAPABILITY.id() {
        return Err((StatusCode::BAD_REQUEST, "unknown capability"));
    }
    let contact = match state.access.find_contact_by_name(name).await {
        Ok(contact) => contact,
        Err(DbErr::RecordNotFound(_)) => {
            if capability_decision(state, name, CHAT_CAPABILITY.id())
                .await?
                .is_some_and(|(decision, _, request_id)| {
                    decision == "denied" && request_id == expected_request_id
                })
            {
                return Ok(());
            }
            return Err((StatusCode::NOT_FOUND, "contact not found"));
        }
        Err(error) => return Err(access_error(error)),
    };
    if !has_chat_access(&contact.requested_access) {
        return Err((
            StatusCode::CONFLICT,
            "capability was not requested by this contact",
        ));
    }
    if contact.requested_access.len() != 1 {
        return Err((
            StatusCode::CONFLICT,
            "request contains other capabilities and cannot be closed as Chat-only",
        ));
    }
    let status = ContactStatus::try_from(contact.status)
        .map_err(|_| (StatusCode::CONFLICT, "contact has invalid status"))?;
    if !matches!(status, ContactStatus::Pending | ContactStatus::Transfered) {
        return Err((
            StatusCode::CONFLICT,
            "only pending capability requests can be denied",
        ));
    }
    let request_id = contact_record_id(state, name, &contact.subject_id).await?;
    if request_id != expected_request_id {
        return Err((StatusCode::CONFLICT, "capability request changed"));
    }
    // Persist the decision before removing the daccess request. If the process
    // stops after this write, the request endpoint still hides the denied item;
    // retrying the same operation remains idempotent.
    record_capability_decision(
        state,
        name,
        CHAT_CAPABILITY.id(),
        "denied",
        &contact.subject_id,
        request_id,
    )
    .await?;
    state
        .access
        .delete_contacts(&[name.to_owned()])
        .await
        .map_err(access_error)?;
    Ok(())
}

async fn reconcile_remote_contact(
    state: &Workspace,
    target: &str,
    remote: &RemoteStatus,
    requested_capabilities: &[String],
    offered_capabilities: &[String],
    current: i64,
) -> Result<(), ApiError> {
    if remote.name != target {
        return Err((
            StatusCode::CONFLICT,
            "remote contact name does not match the requested target",
        ));
    }
    let subject_id = SubjectId::new(remote.subject_id.as_bytes().to_vec())
        .map_err(|_| (StatusCode::CONFLICT, "remote contact subject_id is invalid"))?;
    match state.access.find_contact_by_name(target).await {
        Ok(contact) => {
            if contact.subject_id != subject_id {
                return Err((StatusCode::CONFLICT, "remote contact subject_id changed"));
            }
            match ContactStatus::try_from(contact.status)
                .map_err(|_| (StatusCode::CONFLICT, "contact has invalid status"))?
            {
                ContactStatus::Pending | ContactStatus::Transfered => {
                    state
                        .access
                        .patch_contact(
                            target,
                            ContactPatch::LocalUpdate {
                                status: Some(ContactStatus::Active),
                                alias: None,
                            },
                        )
                        .await
                        .map_err(access_error)?;
                }
                ContactStatus::Active => {}
                ContactStatus::Expired | ContactStatus::Blocked => {
                    return Err((StatusCode::CONFLICT, "local contact cannot be reconciled"));
                }
            }
        }
        Err(DbErr::RecordNotFound(_)) => {
            let (requested_access, offers) =
                selected_access(requested_capabilities, offered_capabilities)?;
            state
                .access
                .create_contact(NewContact {
                    name: target.to_owned(),
                    subject_id,
                    class: String::from("Human"),
                    description: String::new(),
                    requested_access,
                    offers,
                    created_at: current,
                    updated_at: current,
                    expired_after: current + 7 * 86_400,
                })
                .await
                .map_err(access_error)?;
            state
                .access
                .patch_contact(
                    target,
                    ContactPatch::LocalUpdate {
                        status: Some(ContactStatus::Active),
                        alias: None,
                    },
                )
                .await
                .map_err(access_error)?;
        }
        Err(error) => return Err(access_error(error)),
    }
    if offered_capabilities
        .iter()
        .any(|capability| capability == CHAT_CAPABILITY.id())
    {
        ensure_chat_access(state, target).await?;
        let contact = state
            .access
            .find_contact_by_name(target)
            .await
            .map_err(access_error)?;
        record_capability_decision(
            state,
            target,
            CHAT_CAPABILITY.id(),
            "approved",
            &contact.subject_id,
            contact_record_id(state, target, &contact.subject_id).await?,
        )
        .await?;
    }
    update_remote_chat_grant(state, target, remote).await?;
    Ok(())
}

async fn update_remote_chat_grant(
    state: &Workspace,
    target: &str,
    remote: &RemoteStatus,
) -> Result<(), ApiError> {
    if remote.name != target {
        return Err((StatusCode::CONFLICT, "remote contact name changed"));
    }
    let subject_id = SubjectId::new(remote.subject_id.as_bytes().to_vec())
        .map_err(|_| (StatusCode::CONFLICT, "remote contact subject_id is invalid"))?;
    let contact = match state.access.find_contact_by_name(target).await {
        Ok(contact) => contact,
        Err(DbErr::RecordNotFound(_)) => return Ok(()),
        Err(error) => return Err(access_error(error)),
    };
    if contact.subject_id != subject_id {
        return Err((StatusCode::CONFLICT, "remote contact subject_id changed"));
    }
    state
        .update_remote_chat_grant(target, &subject_id, &remote.granted_access)
        .await
        .map_err(access_error)
}

async fn capability_requests_by_status(
    state: &Workspace,
    expired: bool,
) -> Result<Vec<CapabilityRequest>, ApiError> {
    let current = now()?;
    let condition = if expired {
        "status IN (0, 3, 4) AND expired_after <= ?"
    } else {
        "status IN (0, 3) AND expired_after > ?"
    };
    let rows = state
        .access
        .database()
        .query_all_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            format!(
                "SELECT id, name, subject_id, created_at, expired_after, requests FROM contacts \
             WHERE {condition} ORDER BY created_at DESC, name ASC"
            ),
            [current.into()],
        ))
        .await
        .map_err(access_error)?;
    let mut requests = Vec::new();
    for row in rows {
        let requested: RequestedAccess = serde_json::from_str(
            &row.try_get::<String>("", "requests")
                .map_err(access_error)?,
        )
        .map_err(|error| access_error(DbErr::Type(error.to_string())))?;
        if !has_chat_access(&requested) || requested.len() != 1 {
            continue;
        }
        let name: String = row.try_get("", "name").map_err(access_error)?;
        let request_id: i64 = row.try_get("", "id").map_err(access_error)?;
        let subject_id: Vec<u8> = row.try_get("", "subject_id").map_err(access_error)?;
        let created_at: i64 = row.try_get("", "created_at").map_err(access_error)?;
        let expired_after: i64 = row.try_get("", "expired_after").map_err(access_error)?;
        if let Some((_, decision_subject, decided_request_id)) =
            capability_decision(state, &name, CHAT_CAPABILITY.id()).await?
            && decision_subject == subject_id
            && decided_request_id == request_id
        {
            continue;
        }
        requests.push(CapabilityRequest {
            request_id,
            contact_name: name,
            capability_id: CHAT_CAPABILITY.id(),
            capability_version: CHAT_CAPABILITY.version(),
            status: if expired { "expired" } else { "pending" },
            requested_at: created_at,
            expired_after,
        });
    }
    Ok(requests)
}

pub(super) async fn pending_capability_requests(
    state: &Workspace,
) -> Result<Vec<CapabilityRequest>, ApiError> {
    capability_requests_by_status(state, false).await
}

pub(super) async fn expired_capability_requests(
    state: &Workspace,
) -> Result<Vec<CapabilityRequest>, ApiError> {
    capability_requests_by_status(state, true).await
}

pub(crate) async fn capability_requests(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<Visitor>>,
) -> Result<Json<Vec<CapabilityRequest>>, ApiError> {
    require_owner(&state, visitor.as_ref().map(|extension| &extension.0))?;
    Ok(Json(pending_capability_requests(&state).await?))
}

fn decode(row: sea_orm::QueryResult) -> Result<OutboundRequest, ApiError> {
    let requested_capabilities: String = row
        .try_get("", "requested_capabilities")
        .map_err(storage_error)?;
    let offered_capabilities: String = row
        .try_get("", "offered_capabilities")
        .map_err(storage_error)?;
    Ok(OutboundRequest {
        id: row.try_get("", "id").map_err(storage_error)?,
        target_name: row.try_get("", "target_name").map_err(storage_error)?,
        description: row.try_get("", "description").map_err(storage_error)?,
        requested_capabilities: serde_json::from_str(&requested_capabilities)
            .map_err(|error| storage_error(DbErr::Type(error.to_string())))?,
        offered_capabilities: serde_json::from_str(&offered_capabilities)
            .map_err(|error| storage_error(DbErr::Type(error.to_string())))?,
        status: row.try_get("", "status").map_err(storage_error)?,
        expired_after: row.try_get("", "expired_after").map_err(storage_error)?,
        delivery_deadline: row
            .try_get("", "delivery_deadline")
            .map_err(storage_error)?,
        remote_expired_after: row
            .try_get("", "remote_expired_after")
            .map_err(storage_error)?,
        last_checked_at: row.try_get("", "last_checked_at").map_err(storage_error)?,
        error_message: row.try_get("", "error_message").map_err(storage_error)?,
        created_at: row.try_get("", "created_at").map_err(storage_error)?,
        updated_at: row.try_get("", "updated_at").map_err(storage_error)?,
    })
}

const SELECT: &str = "SELECT id, target_name, description, requested_capabilities, offered_capabilities, status, \
    expired_after, delivery_deadline, remote_expired_after, last_checked_at, error_message, created_at, updated_at FROM outbound_contact_requests";

async fn find(state: &Workspace, id: i64) -> Result<OutboundRequest, ApiError> {
    let row = state
        .store
        .db()
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            format!("{SELECT} WHERE id = ?"),
            [id.into()],
        ))
        .await
        .map_err(storage_error)?
        .ok_or((StatusCode::NOT_FOUND, "request not found"))?;
    decode(row)
}

async fn expire(state: &Workspace, current: i64) -> Result<(), ApiError> {
    state
        .store
        .db()
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE outbound_contact_requests SET status = CASE WHEN status = 'queued' THEN 'failed' ELSE 'expired' END, \
         error_message = CASE WHEN status = 'queued' THEN '投递期限结束，未确认是否送达' ELSE error_message END, updated_at = ? \
         WHERE status IN ('queued', 'pending') AND expired_after <= ?",
            [current.into(), current.into()],
        ))
        .await
        .map_err(storage_error)?;
    Ok(())
}

pub(crate) async fn create(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<Visitor>>,
    Json(body): Json<NewRequest>,
) -> Result<(StatusCode, Json<OutboundRequest>), ApiError> {
    require_owner(&state, visitor.as_ref().map(|extension| &extension.0))?;
    let target_name = dhttp_home::normalize_name(body.target_name.trim())
        .ok_or((StatusCode::BAD_REQUEST, "invalid target name"))?;
    let target = target_name.as_str();
    if target == state.owner.name() {
        return Err((StatusCode::BAD_REQUEST, "cannot contact own identity"));
    }
    let description = body.description.trim();
    if description.chars().count() > 80 || description.chars().any(char::is_control) {
        return Err((StatusCode::BAD_REQUEST, "invalid description"));
    }
    selected_access(&body.requested_capabilities, &body.offered_capabilities)?;
    let _send_guard = state.outbound_send.lock().await;
    let current = now()?;
    let delivery_deadline = current + 7 * 86_400;
    expire(&state, current).await?;
    let existing = state.store.db().query_one_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "SELECT id FROM outbound_contact_requests WHERE target_name = ? AND status IN ('queued', 'pending')",
        [target.into()],
    )).await.map_err(storage_error)?;
    if existing.is_some() {
        return Err((StatusCode::CONFLICT, "pending request already exists"));
    }
    let mut nonce = [0_u8; 32];
    getrandom::fill(&mut nonce).map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "random source unavailable",
        )
    })?;
    let application_id = Sha256::digest(nonce)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let requested_capabilities_json =
        serde_json::to_string(&body.requested_capabilities).map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to encode requested capabilities",
            )
        })?;
    let offered_capabilities_json =
        serde_json::to_string(&body.offered_capabilities).map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to encode offered capabilities",
            )
        })?;
    let inserted = state.store.db().execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "INSERT INTO outbound_contact_requests (target_name, description, requested_capabilities, offered_capabilities, \
         application_id, sender_subject_id, status, expired_after, delivery_deadline, next_attempt_at, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, 'queued', ?, ?, ?, ?, ?)",
        [target.into(), description.into(), requested_capabilities_json.into(), offered_capabilities_json.into(),
         application_id.into(), state.owner.subject_id().as_bytes().to_vec().into(), delivery_deadline.into(),
         delivery_deadline.into(), current.into(), current.into(), current.into()],
    )).await.map_err(storage_error)?;
    let id = i64::try_from(inserted.last_insert_id())
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "invalid request id"))?;
    state.wake_worker();
    Ok((StatusCode::ACCEPTED, Json(find(&state, id).await?)))
}

pub(crate) async fn grant(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<Visitor>>,
    Path((name, capability)): Path<(String, String)>,
    Query(query): Query<CapabilityGrantQuery>,
) -> Result<StatusCode, ApiError> {
    require_owner(&state, visitor.as_ref().map(|extension| &extension.0))?;
    let expected = match (query.request_id, query.capability_version.as_deref()) {
        (Some(request_id), Some(version)) => Some((request_id, version)),
        (None, None) => None,
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                "incomplete capability request reference",
            ));
        }
    };
    let _guard = state.contact_write.lock().await;
    grant_capability(&state, &name, &capability, expected).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn revoke(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<Visitor>>,
    Path((name, capability)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    require_owner(&state, visitor.as_ref().map(|extension| &extension.0))?;
    let _guard = state.contact_write.lock().await;
    revoke_capability(&state, &name, &capability).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn deny(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<Visitor>>,
    Path((name, capability)): Path<(String, String)>,
    Query(query): Query<CapabilityDecisionQuery>,
) -> Result<StatusCode, ApiError> {
    require_owner(&state, visitor.as_ref().map(|extension| &extension.0))?;
    if query.capability_version != CHAT_CAPABILITY.version() {
        return Err((StatusCode::CONFLICT, "capability version changed"));
    }
    let _guard = state.contact_write.lock().await;
    deny_capability(&state, &name, &capability, query.request_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn list(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<Visitor>>,
    Query(query): Query<PageQuery>,
) -> Result<Json<RequestPage>, ApiError> {
    require_owner(&state, visitor.as_ref().map(|extension| &extension.0))?;
    let page = query.page;
    if page == 0 {
        return Err((StatusCode::BAD_REQUEST, "invalid page"));
    }
    let page_size = query.page_size.unwrap_or(20);
    if !(1..=100).contains(&page_size) {
        return Err((StatusCode::BAD_REQUEST, "invalid page size"));
    }
    let offset = page
        .checked_sub(1)
        .and_then(|n| n.checked_mul(page_size))
        .and_then(|n| i64::try_from(n).ok())
        .ok_or((StatusCode::BAD_REQUEST, "invalid page"))?;
    expire(&state, now()?).await?;
    let total = state
        .store
        .db()
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS total FROM outbound_contact_requests".to_owned(),
        ))
        .await
        .map_err(storage_error)?
        .ok_or((StatusCode::INTERNAL_SERVER_ERROR, "count unavailable"))?
        .try_get::<i64>("", "total")
        .map_err(storage_error)? as u64;
    let rows = state
        .store
        .db()
        .query_all_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            format!("{SELECT} ORDER BY created_at DESC, id DESC LIMIT ? OFFSET ?"),
            [(page_size as i64).into(), offset.into()],
        ))
        .await
        .map_err(storage_error)?;
    Ok(Json(RequestPage {
        items: rows.into_iter().map(decode).collect::<Result<_, _>>()?,
        total,
        page,
        page_size,
    }))
}

pub(crate) async fn get(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<Visitor>>,
    Path(id): Path<i64>,
) -> Result<Json<OutboundRequest>, ApiError> {
    require_owner(&state, visitor.as_ref().map(|extension| &extension.0))?;
    expire(&state, now()?).await?;
    Ok(Json(find(&state, id).await?))
}

pub(crate) async fn refresh(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<Visitor>>,
    Path(id): Path<i64>,
) -> Result<Json<OutboundRequest>, ApiError> {
    require_owner(&state, visitor.as_ref().map(|extension| &extension.0))?;
    let current = now()?;
    expire(&state, current).await?;
    let request = find(&state, id).await?;
    if !matches!(request.status.as_str(), "queued" | "pending" | "active") {
        return Ok(Json(request));
    }
    state.store.db().execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE outbound_contact_requests SET next_attempt_at = ? WHERE id = ? AND status IN ('queued', 'pending', 'active')",
        [current.into(), id.into()],
    )).await.map_err(storage_error)?;
    state.wake_worker();
    Ok(Json(find(&state, id).await?))
}

pub(crate) async fn delete(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<Visitor>>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    require_owner(&state, visitor.as_ref().map(|extension| &extension.0))?;
    let _send_guard = state.outbound_send.lock().await;
    let current = now()?;
    let deleted = state
        .store
        .db()
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "DELETE FROM outbound_contact_requests WHERE id = ? AND (lease_until IS NULL OR lease_until <= ?)",
            [id.into(), current.into()],
        ))
        .await
        .map_err(storage_error)?;
    if deleted.rows_affected() == 0 {
        let existing = state
            .store
            .db()
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT id FROM outbound_contact_requests WHERE id = ?",
                [id.into()],
            ))
            .await
            .map_err(storage_error)?;
        return Err(if existing.is_some() {
            (StatusCode::CONFLICT, "request is being delivered")
        } else {
            (StatusCode::NOT_FOUND, "request not found")
        });
    }
    Ok(StatusCode::NO_CONTENT)
}
