use std::{collections::HashMap, sync::Arc};

use access_control::{ContactStatus, Visitor};
use axum::{
    Extension, Json,
    extract::{Path, State},
};
use http::StatusCode;
use sea_orm::{ConnectionTrait, DatabaseBackend, DbErr, Statement};
use serde::Serialize;

use super::{Workspace, settings::require_owner};
use crate::chat::CHAT_CAPABILITY;

type ApiError = (StatusCode, &'static str);

#[derive(Serialize)]
pub(crate) struct DirectoryEntry {
    name: String,
    saved: bool,
    chat_available: bool,
    remote_chat_granted: Option<bool>,
}

fn storage_error(error: DbErr) -> ApiError {
    tracing::error!(error = %error, "Workspace contact directory failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "contact directory unavailable",
    )
}

pub(crate) async fn list(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<Visitor>>,
) -> Result<Json<Vec<DirectoryEntry>>, ApiError> {
    require_owner(&state, visitor.as_ref().map(|extension| &extension.0))?;
    let chat = state
        .chat
        .read()
        .await
        .as_ref()
        .and_then(std::sync::Weak::upgrade)
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "Chat module unavailable"))?;
    let remote_grants: HashMap<(String, Vec<u8>), bool> = chat
        .remote_chat_grants()
        .await
        .map_err(storage_error)?
        .into_iter()
        .map(|(name, subject_id, granted)| ((name, subject_id), granted))
        .collect();
    let saved: HashMap<String, (i64, Vec<u8>)> = state
        .store
        .db()
        .query_all_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT contact_name, contact_id, subject_id FROM saved_contacts".to_owned(),
        ))
        .await
        .map_err(storage_error)?
        .into_iter()
        .map(|row| {
            Ok((
                row.try_get("", "contact_name")?,
                (
                    row.try_get("", "contact_id")?,
                    row.try_get("", "subject_id")?,
                ),
            ))
        })
        .collect::<Result<_, DbErr>>()
        .map_err(storage_error)?;
    let approved_chat: HashMap<String, (i64, Vec<u8>)> = state
        .store
        .db()
        .query_all_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT contact_name, request_id, subject_id FROM capability_decisions \
             WHERE capability_id = ? AND descriptor_version = ? AND decision = 'approved'",
            [
                CHAT_CAPABILITY.id().into(),
                CHAT_CAPABILITY.version().into(),
            ],
        ))
        .await
        .map_err(storage_error)?
        .into_iter()
        .map(|row| {
            Ok((
                row.try_get("", "contact_name")?,
                (
                    row.try_get("", "request_id")?,
                    row.try_get("", "subject_id")?,
                ),
            ))
        })
        .collect::<Result<_, DbErr>>()
        .map_err(storage_error)?;
    let rows = state
        .access
        .database()
        .query_all_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT c.id, c.name, c.subject_id, c.status, \
             EXISTS (SELECT 1 FROM access_rules AS r \
               WHERE r.grantee_type = 0 AND r.grantee = c.name \
                 AND r.method = 'POST' AND r.api = '/std/message' AND r.effect = 'allow') \
             AS local_chat FROM contacts AS c ORDER BY c.name ASC"
                .to_owned(),
        ))
        .await
        .map_err(storage_error)?;
    let mut entries = Vec::new();
    for row in rows {
        let id: i64 = row.try_get("", "id").map_err(storage_error)?;
        let name: String = row.try_get("", "name").map_err(storage_error)?;
        let subject_id: Vec<u8> = row.try_get("", "subject_id").map_err(storage_error)?;
        let status: i32 = row.try_get("", "status").map_err(storage_error)?;
        let status = ContactStatus::try_from(status)
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "invalid contact status"))?;
        let local_chat: i64 = row.try_get("", "local_chat").map_err(storage_error)?;
        let is_saved = saved.get(&name).is_some_and(|(saved_id, saved_subject)| {
            *saved_id == id && saved_subject == &subject_id
        });
        let local_chat_approved = local_chat == 1
            && approved_chat
                .get(&name)
                .is_some_and(|(request_id, approved_subject)| {
                    *request_id == id && approved_subject == &subject_id
                });
        let remote_chat_granted = remote_grants.get(&(name.clone(), subject_id)).copied();
        let effective_chat = status == ContactStatus::Active
            && (local_chat_approved || remote_chat_granted == Some(true));
        // A blocked identity remains visible so its owner can restore or delete it.
        if is_saved || effective_chat || status == ContactStatus::Blocked {
            entries.push(DirectoryEntry {
                name,
                saved: is_saved,
                chat_available: effective_chat,
                remote_chat_granted,
            });
        }
    }
    Ok(Json(entries))
}

/// Confirm that a Chat rule still belongs to the currently verified contact
/// record, rather than a previous identity that used the same name.
pub(crate) async fn approved_chat_grant(
    state: &Workspace,
    visitor: &Visitor,
) -> Result<bool, DbErr> {
    let row = state
        .access
        .database()
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT c.id FROM contacts AS c WHERE c.name = ? AND c.subject_id = ? \
             AND EXISTS (SELECT 1 FROM access_rules AS r WHERE r.grantee_type = 0 \
               AND r.grantee = c.name AND r.method = 'POST' \
               AND r.api = '/std/message' AND r.effect = 'allow')",
            [
                visitor.name().into(),
                visitor.subject_id().as_bytes().to_vec().into(),
            ],
        ))
        .await?;
    let Some(row) = row else {
        return Ok(false);
    };
    let contact_id: i64 = row.try_get("", "id")?;
    Ok(state
        .store
        .db()
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT 1 FROM capability_decisions WHERE contact_name = ? \
             AND capability_id = ? AND descriptor_version = ? AND decision = 'approved' \
             AND request_id = ? AND subject_id = ?",
            [
                visitor.name().into(),
                CHAT_CAPABILITY.id().into(),
                CHAT_CAPABILITY.version().into(),
                contact_id.into(),
                visitor.subject_id().as_bytes().to_vec().into(),
            ],
        ))
        .await?
        .is_some())
}

pub(crate) async fn save(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<Visitor>>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_owner(&state, visitor.as_ref().map(|extension| &extension.0))?;
    let _guard = state.contact_write.lock().await;
    let contact = state
        .access
        .find_contact_by_name(&name)
        .await
        .map_err(|error| match error {
            DbErr::RecordNotFound(_) => (StatusCode::NOT_FOUND, "contact not found"),
            error => storage_error(error),
        })?;
    let row = state
        .access
        .database()
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT id FROM contacts WHERE name = ? AND subject_id = ?",
            [
                name.as_str().into(),
                contact.subject_id.as_bytes().to_vec().into(),
            ],
        ))
        .await
        .map_err(storage_error)?
        .ok_or((StatusCode::CONFLICT, "contact identity changed"))?;
    let id: i64 = row.try_get("", "id").map_err(storage_error)?;
    state
        .store
        .db()
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "INSERT INTO saved_contacts (contact_name, contact_id, subject_id, saved_at) \
             VALUES (?, ?, ?, CAST(strftime('%s', 'now') AS INTEGER)) \
             ON CONFLICT(contact_name) DO UPDATE SET \
             contact_id = excluded.contact_id, subject_id = excluded.subject_id, saved_at = excluded.saved_at",
            [name.into(), id.into(), contact.subject_id.as_bytes().to_vec().into()],
        ))
        .await
        .map_err(storage_error)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn unsave(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<Visitor>>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_owner(&state, visitor.as_ref().map(|extension| &extension.0))?;
    let _guard = state.contact_write.lock().await;
    state
        .store
        .db()
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "DELETE FROM saved_contacts WHERE contact_name = ?",
            [name.into()],
        ))
        .await
        .map_err(storage_error)?;
    Ok(StatusCode::NO_CONTENT)
}
