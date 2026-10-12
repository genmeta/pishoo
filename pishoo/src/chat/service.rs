use std::time::{SystemTime, UNIX_EPOCH};

use access_control::{ContactStatus, SubjectId, Visitor};
use http::StatusCode;
use sea_orm::{ConnectionTrait, DatabaseBackend, DbErr, Statement, TransactionTrait};
use serde::Serialize;

use super::{
    CHAT_CAPABILITY, Chat,
    message::{MessageQuery, MessageSubmission},
};

pub(crate) type ServiceError = (StatusCode, &'static str);

#[derive(Clone, Debug, Serialize)]
pub(crate) struct LocalMessage {
    pub(crate) id: String,
    pub(crate) client_message_id: String,
    pub(crate) remote_message_id: Option<String>,
    pub(crate) direction: String,
    pub(crate) state: String,
    pub(crate) sender: String,
    pub(crate) recipient: String,
    pub(crate) text: String,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
    pub(crate) error_message: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ConversationPage {
    pub(crate) items: Vec<LocalMessage>,
    pub(crate) next_cursor: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct CapabilityState {
    pub(crate) capability: &'static str,
    pub(crate) status: &'static str,
    pub(crate) contact_status: ContactStatus,
    /// Whether the local profile may enqueue messages for this contact.
    pub(crate) can_send: bool,
    pub(crate) can_receive: bool,
    /// Most recently observed permission on the remote profile. `None` means
    /// that this profile has not observed a `/std/contact/self` response yet.
    pub(crate) remote_grant: Option<bool>,
    pub(crate) endpoints: &'static [super::capabilities::CapabilityEndpoint],
}

pub(crate) fn require_owner(state: &Chat, visitor: Option<&Visitor>) -> Result<(), ServiceError> {
    state
        .owner
        .ensure(visitor)
        .map_err(|_| (StatusCode::FORBIDDEN, "verified owner identity is required"))
}

pub(crate) async fn resolve_target(state: &Chat, raw_name: &str) -> Result<String, ServiceError> {
    let (target, status) = resolve_contact(state, raw_name).await?;
    if status != ContactStatus::Active {
        return Err((StatusCode::FORBIDDEN, "contact is not active for Chat"));
    }
    Ok(target)
}

async fn resolve_contact(
    state: &Chat,
    raw_name: &str,
) -> Result<(String, ContactStatus), ServiceError> {
    let name = dhttp_home::normalize_name(raw_name.trim())
        .ok_or((StatusCode::BAD_REQUEST, "invalid conversation name"))?;
    let target = name.as_str();
    if target == state.owner.name() {
        return Err((
            StatusCode::BAD_REQUEST,
            "cannot open a conversation with the local identity",
        ));
    }
    let contact = state
        .access
        .find_contact_by_name(target)
        .await
        .map_err(|error| match error {
            DbErr::RecordNotFound(_) => (StatusCode::NOT_FOUND, "contact not found"),
            error => {
                tracing::error!(error = %error, %target, "Chat contact lookup failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Chat contact lookup failed",
                )
            }
        })?;
    let status = ContactStatus::try_from(contact.status)
        .map_err(|_| (StatusCode::CONFLICT, "contact has an invalid status"))?;
    Ok((target.to_owned(), status))
}

pub(crate) async fn capability_state(
    state: &Chat,
    raw_name: &str,
) -> Result<CapabilityState, ServiceError> {
    let (target, contact_status) = resolve_contact(state, raw_name).await?;
    let contact = state
        .access
        .find_contact_by_name(&target)
        .await
        .map_err(storage_error)?;
    let remote_grant = remote_chat_granted(state, &target, &contact.subject_id).await?;
    let requested_chat = contact.requested_access.contains_key("/std/message");
    let can_send =
        contact_status == ContactStatus::Active && (requested_chat || remote_grant == Some(true));
    let can_receive =
        contact_status == ContactStatus::Active && has_local_chat_rule(state, &target).await?;
    let status = match contact_status {
        ContactStatus::Active if can_send || can_receive => "available",
        ContactStatus::Blocked | ContactStatus::Expired => "blocked",
        _ => "waiting",
    };
    Ok(CapabilityState {
        capability: CHAT_CAPABILITY.id(),
        status,
        contact_status,
        can_send,
        can_receive,
        remote_grant,
        endpoints: CHAT_CAPABILITY.endpoints(),
    })
}

async fn has_local_chat_rule(state: &Chat, target: &str) -> Result<bool, ServiceError> {
    let row = state
        .access
        .database()
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS count FROM access_rules WHERE grantee_type = 0 AND grantee = ? AND api = ? AND effect = 'allow' AND method = 'POST'",
            [target.into(), "/std/message".into()],
        ))
        .await
        .map_err(storage_error)?
        .ok_or((
            StatusCode::INTERNAL_SERVER_ERROR,
            "Chat capability state unavailable",
        ))?;
    let count: i64 = row.try_get("", "count").map_err(storage_error)?;
    Ok(count == 1)
}

async fn remote_chat_granted(
    state: &Chat,
    target: &str,
    subject_id: &SubjectId,
) -> Result<Option<bool>, ServiceError> {
    let row = state
        .store
        .db()
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT remote_message_granted FROM chat_capability_state WHERE contact_name = ? AND subject_id = ?",
            [target.into(), subject_id.as_bytes().to_vec().into()],
        ))
        .await
        .map_err(storage_error)?;
    match row {
        None => Ok(None),
        Some(row) => Ok(Some(
            row.try_get::<i64>("", "remote_message_granted")
                .map_err(storage_error)?
                == 1,
        )),
    }
}

pub(crate) async fn list_messages(
    state: &Chat,
    target: &str,
    after: Option<String>,
    limit: Option<u16>,
) -> Result<ConversationPage, ServiceError> {
    let query = MessageQuery::new(after, limit)
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid message query"))?;
    let after = query
        .after
        .as_deref()
        .map(str::parse::<i64>)
        .transpose()
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid message cursor"))?
        .unwrap_or(0);
    let rows = state
        .store
        .db()
        .query_all_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT id FROM chat_messages \
             WHERE contact_name = ? AND id > ? ORDER BY id ASC LIMIT ?",
            [
                target.into(),
                after.into(),
                (i64::from(query.limit) + 1).into(),
            ],
        ))
        .await
        .map_err(storage_error)?;
    let has_more = rows.len() > usize::from(query.limit);
    let rows = rows.into_iter().take(usize::from(query.limit));
    let mut items = Vec::new();
    for row in rows {
        items.push(read_local_message(state, row.try_get("", "id").map_err(storage_error)?).await?);
    }
    let next_cursor = has_more
        .then(|| items.last().map(|item| item.id.clone()))
        .flatten();
    Ok(ConversationPage { items, next_cursor })
}

pub(crate) async fn send_message(
    state: &Chat,
    target: &str,
    text: String,
) -> Result<LocalMessage, ServiceError> {
    if text.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "message text is required"));
    }
    let contact = state
        .access
        .find_contact_by_name(target)
        .await
        .map_err(storage_error)?;
    if ContactStatus::try_from(contact.status) != Ok(ContactStatus::Active) {
        return Err((StatusCode::FORBIDDEN, "contact is not active for Chat"));
    }
    let requested_chat = contact.requested_access.contains_key("/std/message");
    let remote_grant = remote_chat_granted(state, target, &contact.subject_id).await?;
    if !requested_chat && remote_grant != Some(true) {
        return Err((
            StatusCode::FORBIDDEN,
            "Chat delivery was not requested or granted for this contact",
        ));
    }
    let submission = MessageSubmission {
        client_message_id: next_client_message_id()?,
        text,
    };
    submission
        .validate()
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid message submission"))?;
    let current = now()?;
    let local_id = insert_pending(state, target, &contact.subject_id, &submission, current).await?;
    state.wake_worker();
    read_local_message(state, local_id).await
}

pub(crate) async fn requeue_message(
    state: &Chat,
    target: &str,
    raw_id: &str,
) -> Result<LocalMessage, ServiceError> {
    let contact = state
        .access
        .find_contact_by_name(target)
        .await
        .map_err(storage_error)?;
    if ContactStatus::try_from(contact.status) != Ok(ContactStatus::Active) {
        return Err((StatusCode::FORBIDDEN, "contact is not active for Chat"));
    }
    let requested_chat = contact.requested_access.contains_key("/std/message");
    let remote_grant = remote_chat_granted(state, target, &contact.subject_id).await?;
    if !requested_chat && remote_grant != Some(true) {
        return Err((
            StatusCode::FORBIDDEN,
            "Chat delivery was not requested or granted for this contact",
        ));
    }
    let local_id = raw_id
        .parse::<i64>()
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid local message id"))?;
    let message = read_local_message(state, local_id).await?;
    if !matches!(message.state.as_str(), "failed" | "blocked") || message.recipient != target {
        return Err((
            StatusCode::CONFLICT,
            "only failed or blocked messages for this conversation can be requeued",
        ));
    }
    let stored_subject = state
        .store
        .db()
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT recipient_subject_id FROM chat_messages WHERE id = ? AND contact_name = ? AND direction = 'outgoing'",
            [local_id.into(), target.into()],
        ))
        .await
        .map_err(storage_error)?
        .ok_or((StatusCode::CONFLICT, "outgoing message is unavailable"))?
        .try_get::<Option<Vec<u8>>>("", "recipient_subject_id")
        .map_err(storage_error)?;
    if stored_subject.as_deref() != Some(contact.subject_id.as_bytes()) {
        return Err((
            StatusCode::CONFLICT,
            "contact identity changed since this message was queued",
        ));
    }
    let submission = MessageSubmission {
        client_message_id: message.client_message_id,
        text: message.text,
    };
    submission
        .validate()
        .map_err(|_| (StatusCode::CONFLICT, "stored message is no longer valid"))?;
    let current = now()?;
    let transaction = state.store.db().begin().await.map_err(storage_error)?;
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE chat_messages SET state = 'queued', updated_at = ?, next_attempt_at = ?, error_message = NULL WHERE id = ?",
            [current.into(), current.into(), local_id.into()],
        ))
        .await
        .map_err(storage_error)?;
    let job_update = transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE chat_jobs SET state = 'queued', available_at = ?, lease_token = NULL, lease_until = NULL, last_error = NULL, updated_at = ? WHERE kind = 'send' AND message_id = ?",
            [current.into(), current.into(), local_id.into()],
        ))
        .await
        .map_err(storage_error)?;
    if job_update.rows_affected() != 1 {
        transaction.rollback().await.map_err(storage_error)?;
        return Err((
            StatusCode::CONFLICT,
            "message delivery job is no longer available",
        ));
    }
    transaction.commit().await.map_err(storage_error)?;
    state.wake_worker();
    read_local_message(state, local_id).await
}

async fn insert_pending(
    state: &Chat,
    target: &str,
    subject_id: &SubjectId,
    submission: &MessageSubmission,
    current: i64,
) -> Result<i64, ServiceError> {
    let transaction = state.store.db().begin().await.map_err(storage_error)?;
    let result = transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "INSERT INTO chat_messages (contact_name, client_message_id, direction, state, sender_name, recipient_name, recipient_subject_id, text, next_attempt_at, created_at, updated_at) VALUES (?, ?, 'outgoing', 'queued', ?, ?, ?, ?, ?, ?, ?)",
            [
                target.into(),
                submission.client_message_id.as_str().into(),
                state.owner.name().into(),
                target.into(),
                subject_id.as_bytes().to_vec().into(),
                submission.text.as_str().into(),
                current.into(),
                current.into(),
                current.into(),
            ],
        ))
        .await
        .map_err(storage_error)?;
    let id = i64::try_from(result.last_insert_id()).map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "invalid local message id",
        )
    })?;
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "INSERT INTO chat_conversations (contact_name, updated_at) VALUES (?, ?) ON CONFLICT(contact_name) DO UPDATE SET updated_at = excluded.updated_at",
            [target.into(), current.into()],
        ))
        .await
        .map_err(storage_error)?;
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "INSERT INTO chat_jobs (kind, contact_name, message_id, state, available_at, created_at, updated_at) VALUES ('send', ?, ?, 'queued', ?, ?, ?)",
            [
                target.into(),
                id.into(),
                current.into(),
                current.into(),
                current.into(),
            ],
        ))
        .await
        .map_err(storage_error)?;
    transaction.commit().await.map_err(storage_error)?;
    Ok(id)
}

async fn read_local_message(state: &Chat, id: i64) -> Result<LocalMessage, ServiceError> {
    let row = state
        .store
        .db()
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT id, client_message_id, remote_message_id, direction, state, sender_name, recipient_name, text, created_at, updated_at, error_message FROM chat_messages WHERE id = ?",
            [id.into()],
        ))
        .await
        .map_err(storage_error)?
        .ok_or((StatusCode::INTERNAL_SERVER_ERROR, "local message was not stored"))?;
    decode_local_message(row)
}

fn decode_local_message(row: sea_orm::QueryResult) -> Result<LocalMessage, ServiceError> {
    Ok(LocalMessage {
        id: row
            .try_get::<i64>("", "id")
            .map_err(storage_error)?
            .to_string(),
        client_message_id: row
            .try_get("", "client_message_id")
            .map_err(storage_error)?,
        remote_message_id: row
            .try_get("", "remote_message_id")
            .map_err(storage_error)?,
        direction: row.try_get("", "direction").map_err(storage_error)?,
        state: row.try_get("", "state").map_err(storage_error)?,
        sender: row.try_get("", "sender_name").map_err(storage_error)?,
        recipient: row.try_get("", "recipient_name").map_err(storage_error)?,
        text: row.try_get("", "text").map_err(storage_error)?,
        created_at: row.try_get("", "created_at").map_err(storage_error)?,
        updated_at: row.try_get("", "updated_at").map_err(storage_error)?,
        error_message: row.try_get("", "error_message").map_err(storage_error)?,
    })
}

pub(crate) fn storage_error(error: DbErr) -> ServiceError {
    tracing::error!(error = %error, "Chat local storage failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "Chat local storage failed",
    )
}

pub(crate) fn now() -> Result<i64, ServiceError> {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "invalid system clock"))?
            .as_secs(),
    )
    .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "invalid system clock"))
}

fn next_client_message_id() -> Result<String, ServiceError> {
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce).map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "random source unavailable",
        )
    })?;
    Ok(nonce.iter().map(|byte| format!("{byte:02x}")).collect())
}
