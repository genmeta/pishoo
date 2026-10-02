use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use access_control::{ContactStatus, Visitor};
use axum::{
    Extension, Json,
    extract::{State, rejection::JsonRejection},
};
use http::StatusCode;
use sea_orm::{ConnectionTrait, DatabaseBackend, DbErr, Statement, TransactionTrait};

use super::{
    Chat,
    message::{MessageEnvelope, MessageSubmission},
};

type ApiError = (StatusCode, &'static str);

fn storage_error(error: DbErr) -> ApiError {
    tracing::error!(error = %error, "Chat message storage failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "Chat message storage failed",
    )
}

fn access_error(error: DbErr) -> ApiError {
    tracing::error!(error = %error, "Chat contact lookup failed");
    match error {
        DbErr::RecordNotFound(_) => (StatusCode::NOT_FOUND, "contact not found"),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Chat contact lookup failed",
        ),
    }
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

async fn require_chat_contact(state: &Chat, visitor: Option<&Visitor>) -> Result<String, ApiError> {
    let visitor = visitor.ok_or((
        StatusCode::UNAUTHORIZED,
        "verified visitor identity is required",
    ))?;
    let contact = state
        .access
        .find_contact_by_name(visitor.name())
        .await
        .map_err(access_error)?;
    if contact.subject_id != *visitor.subject_id() {
        return Err((
            StatusCode::FORBIDDEN,
            "contact subject_id no longer matches the verified visitor",
        ));
    }
    if ContactStatus::try_from(contact.status)
        .map_err(|_| (StatusCode::CONFLICT, "contact has an invalid status"))?
        != ContactStatus::Active
    {
        return Err((StatusCode::FORBIDDEN, "contact is not active"));
    }
    Ok(visitor.name().to_owned())
}

async fn read_message(db: &impl ConnectionTrait, id: i64) -> Result<MessageEnvelope, ApiError> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT id, client_message_id, sender_name, recipient_name, text, created_at \
             FROM chat_messages WHERE id = ?",
            [id.into()],
        ))
        .await
        .map_err(storage_error)?
        .ok_or((StatusCode::INTERNAL_SERVER_ERROR, "message was not stored"))?;
    Ok(MessageEnvelope {
        id: row
            .try_get::<i64>("", "id")
            .map_err(storage_error)?
            .to_string(),
        client_message_id: row
            .try_get("", "client_message_id")
            .map_err(storage_error)?,
        sender: row.try_get("", "sender_name").map_err(storage_error)?,
        recipient: row.try_get("", "recipient_name").map_err(storage_error)?,
        text: row.try_get("", "text").map_err(storage_error)?,
        created_at: row.try_get("", "created_at").map_err(storage_error)?,
    })
}

pub(crate) async fn post(
    State(state): State<Arc<Chat>>,
    visitor: Option<Extension<Visitor>>,
    body: Result<Json<MessageSubmission>, JsonRejection>,
) -> Result<(StatusCode, Json<MessageEnvelope>), ApiError> {
    let sender =
        require_chat_contact(&state, visitor.as_ref().map(|extension| &extension.0)).await?;
    let body = body
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid message submission"))?
        .0;
    body.validate()
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid message submission"))?;
    let current = now()?;
    let transaction = state.store.db().begin().await.map_err(storage_error)?;
    if let Some(row) = transaction
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT id, text FROM chat_messages \
             WHERE contact_name = ? AND client_message_id = ?",
            [
                sender.as_str().into(),
                body.client_message_id.as_str().into(),
            ],
        ))
        .await
        .map_err(storage_error)?
    {
        let existing_text: String = row.try_get("", "text").map_err(storage_error)?;
        if existing_text != body.text {
            return Err((
                StatusCode::CONFLICT,
                "client message id was already used for different text",
            ));
        }
        let message =
            read_message(&transaction, row.try_get("", "id").map_err(storage_error)?).await?;
        transaction.commit().await.map_err(storage_error)?;
        return Ok((StatusCode::OK, Json(message)));
    }
    let recipient = state.owner.name().to_owned();
    let result = transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "INSERT INTO chat_messages \
             (contact_name, client_message_id, direction, state, sender_name, recipient_name, text, created_at, updated_at) \
             VALUES (?, ?, 'incoming', 'received', ?, ?, ?, ?, ?)",
            [
                sender.as_str().into(),
                body.client_message_id.as_str().into(),
                sender.as_str().into(),
                recipient.into(),
                body.text.as_str().into(),
                current.into(),
                current.into(),
            ],
        ))
        .await
        .map_err(storage_error)?;
    let id = i64::try_from(result.last_insert_id())
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "invalid message id"))?;
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "INSERT INTO chat_conversations (contact_name, updated_at) VALUES (?, ?) \
             ON CONFLICT(contact_name) DO UPDATE SET updated_at = excluded.updated_at",
            [sender.into(), current.into()],
        ))
        .await
        .map_err(storage_error)?;
    let message = read_message(&transaction, id).await?;
    transaction.commit().await.map_err(storage_error)?;
    Ok((StatusCode::OK, Json(message)))
}
