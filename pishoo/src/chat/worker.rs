use std::{
    sync::{
        Arc, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use bytes::Bytes;
use http::{Method, StatusCode};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement, TransactionTrait};
use serde_json::from_slice;
use tokio::{sync::Notify, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use super::{
    Chat,
    message::{MessageEnvelope, MessageSubmission},
    outbound::REMOTE_IDENTITY_CHANGED,
};

const LEASE_SECONDS: i64 = 60;
const MAX_BACKOFF_SECONDS: i64 = 3_600;
static NEXT_LEASE: AtomicU64 = AtomicU64::new(1);

struct Job {
    id: i64,
    contact_name: String,
    message_id: Option<i64>,
    attempt_count: i64,
    lease_token: String,
}

pub(crate) fn spawn(
    state: Weak<Chat>,
    notify: Arc<Notify>,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        run(state, notify, shutdown).await;
    })
}

async fn run(state: Weak<Chat>, notify: Arc<Notify>, shutdown: CancellationToken) {
    loop {
        if shutdown.is_cancelled() {
            return;
        }
        let Some(state) = state.upgrade() else {
            return;
        };
        if let Err(error) = recover_expired_jobs(&state).await {
            tracing::warn!(%error, "Chat worker could not recover expired jobs");
        }
        match claim_next_job(&state).await {
            Ok(Some(job)) => {
                process_job(&state, job).await;
                continue;
            }
            Ok(None) => {}
            Err(error) => tracing::warn!(%error, "Chat worker could not claim a job"),
        }
        let next_wake = match next_wake_delay(&state).await {
            Ok(delay) => delay,
            Err(error) => {
                tracing::warn!(%error, "Chat worker could not schedule its next wake-up");
                Some(Duration::from_secs(1))
            }
        };
        drop(state);
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = notify.notified() => {},
            _ = async {
                if let Some(delay) = next_wake {
                    tokio::time::sleep(delay).await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {},
        }
    }
}

async fn next_wake_delay(state: &Chat) -> Result<Option<Duration>, String> {
    let now = super::service::now().map_err(|error| error.1.to_owned())?;
    let row = state
        .store
        .db()
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT MIN(wake_at) AS wake_at FROM ( SELECT available_at AS wake_at FROM chat_jobs WHERE state = 'queued' UNION ALL SELECT lease_until AS wake_at FROM chat_jobs WHERE state = 'running' AND lease_until IS NOT NULL )"
                .to_owned(),
        ))
        .await
        .map_err(|error| error.to_string())?;
    let wake_at: Option<i64> = row
        .ok_or_else(|| String::from("chat worker wake-up query returned no row"))?
        .try_get("", "wake_at")
        .map_err(|error| error.to_string())?;
    Ok(wake_at.map(|wake_at| {
        Duration::from_secs(u64::try_from(wake_at.saturating_sub(now)).unwrap_or(0))
    }))
}

async fn recover_expired_jobs(state: &Chat) -> Result<(), String> {
    let now = super::service::now().map_err(|error| error.1.to_owned())?;
    state
        .store
        .db()
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE chat_jobs SET state = 'queued', lease_token = NULL, lease_until = NULL, updated_at = ? WHERE state = 'running' AND lease_until IS NOT NULL AND lease_until <= ?",
            [now.into(), now.into()],
        ))
        .await
        .map_err(|error| error.to_string())?;
    state
        .store
        .db()
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE chat_messages SET state = 'queued', updated_at = ? WHERE state = 'sending' AND id NOT IN (SELECT message_id FROM chat_jobs WHERE state = 'running' AND message_id IS NOT NULL)",
            [now.into()],
        ))
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

async fn claim_next_job(state: &Chat) -> Result<Option<Job>, String> {
    let now = super::service::now().map_err(|error| error.1.to_owned())?;
    let transaction = state
        .store
        .db()
        .begin()
        .await
        .map_err(|error| error.to_string())?;
    let row = transaction
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT queued.id, queued.kind, queued.contact_name, queued.message_id, queued.attempt_count \
             FROM chat_jobs AS queued \
             WHERE queued.state = 'queued' AND queued.available_at <= ? \
               AND NOT EXISTS ( \
                   SELECT 1 FROM chat_jobs AS running \
                   WHERE running.kind = 'send' AND running.state = 'running' \
                     AND running.contact_name = queued.contact_name \
               ) \
             ORDER BY queued.available_at ASC, queued.id ASC LIMIT 1",
            [now.into()],
        ))
        .await
        .map_err(|error| error.to_string())?;
    let Some(row) = row else {
        transaction
            .commit()
            .await
            .map_err(|error| error.to_string())?;
        return Ok(None);
    };
    let id: i64 = row.try_get("", "id").map_err(|error| error.to_string())?;
    let lease_token = format!(
        "{}-{}",
        std::process::id(),
        NEXT_LEASE.fetch_add(1, Ordering::Relaxed)
    );
    let lease_until = now + LEASE_SECONDS;
    let claimed = transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE chat_jobs SET state = 'running', lease_token = ?, lease_until = ?, updated_at = ? WHERE id = ? AND state = 'queued'",
            [lease_token.as_str().into(), lease_until.into(), now.into(), id.into()],
        ))
        .await
        .map_err(|error| error.to_string())?;
    if claimed.rows_affected() != 1 {
        transaction
            .commit()
            .await
            .map_err(|error| error.to_string())?;
        return Ok(None);
    }
    transaction
        .commit()
        .await
        .map_err(|error| error.to_string())?;
    Ok(Some(Job {
        id,
        contact_name: row
            .try_get("", "contact_name")
            .map_err(|error| error.to_string())?,
        message_id: row
            .try_get("", "message_id")
            .map_err(|error| error.to_string())?,
        attempt_count: row
            .try_get("", "attempt_count")
            .map_err(|error| error.to_string())?,
        lease_token,
    }))
}

async fn process_job(state: &Chat, job: Job) {
    let result = process_send(state, &job).await;
    if let Err(error) = result {
        tracing::warn!(job_id = job.id, contact = %job.contact_name, %error, "Chat worker job failed");
    }
}

async fn process_send(state: &Chat, job: &Job) -> Result<(), String> {
    let message_id = job
        .message_id
        .ok_or_else(|| String::from("send job has no message"))?;
    let row = state
        .store
        .db()
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT client_message_id, text, recipient_subject_id FROM chat_messages WHERE id = ?",
            [message_id.into()],
        ))
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| String::from("queued message was deleted"))?;
    let client_message_id: String = row
        .try_get("", "client_message_id")
        .map_err(|error| error.to_string())?;
    let text: String = row.try_get("", "text").map_err(|error| error.to_string())?;
    let recipient_subject_id: Option<Vec<u8>> = row
        .try_get("", "recipient_subject_id")
        .map_err(|error| error.to_string())?;
    let contact = match state.access.find_contact_by_name(&job.contact_name).await {
        Ok(contact) => contact,
        Err(sea_orm::DbErr::RecordNotFound(_)) => {
            return finish_message(state, job, message_id, "blocked", "contact was removed").await;
        }
        Err(error) => return retry_job(state, job, message_id, error.to_string()).await,
    };
    let status = access_control::ContactStatus::try_from(contact.status)
        .map_err(|_| String::from("contact has invalid status"))?;
    if recipient_subject_id.as_deref() != Some(contact.subject_id.as_bytes()) {
        return finish_message(
            state,
            job,
            message_id,
            "blocked",
            "contact identity changed since message was queued",
        )
        .await;
    }
    if status != access_control::ContactStatus::Active {
        return finish_message(
            state,
            job,
            message_id,
            "blocked",
            "Chat capability is not active",
        )
        .await;
    }
    let submission = MessageSubmission {
        client_message_id,
        text,
    };
    let payload =
        serde_json::to_vec(&submission).map_err(|_| String::from("failed to encode message"))?;
    mark_sending(state, job, message_id).await?;
    let Some(transport) = state.outbound.read().await.clone() else {
        return retry_job(
            state,
            job,
            message_id,
            String::from("Chat outbound connector unavailable"),
        )
        .await;
    };
    let response = transport
        .request(
            &job.contact_name,
            &contact.subject_id,
            Method::POST,
            "/std/message",
            Bytes::from(payload),
        )
        .await;
    match response {
        Ok(response) if response.status == StatusCode::OK => {
            let remote: MessageEnvelope = match from_slice(&response.body) {
                Ok(remote) => remote,
                Err(_) => {
                    return finish_message(
                        state,
                        job,
                        message_id,
                        "failed",
                        "remote Chat response was invalid",
                    )
                    .await;
                }
            };
            let now = super::service::now().map_err(|error| error.1.to_owned())?;
            let transaction = state
                .store
                .db()
                .begin()
                .await
                .map_err(|error| error.to_string())?;
            transaction.execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "UPDATE chat_messages SET state = 'sent', remote_message_id = ?, delivered_at = ?, updated_at = ?, error_message = NULL WHERE id = ? AND EXISTS (SELECT 1 FROM chat_jobs WHERE id = ? AND lease_token = ?)",
                [remote.id.into(), now.into(), now.into(), message_id.into(), job.id.into(), job.lease_token.as_str().into()],
            )).await.map_err(|error| error.to_string())?;
            transaction.execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "UPDATE chat_jobs SET state = 'completed', lease_token = NULL, lease_until = NULL, updated_at = ? WHERE id = ? AND lease_token = ?",
                [now.into(), job.id.into(), job.lease_token.as_str().into()],
            )).await.map_err(|error| error.to_string())?;
            transaction
                .commit()
                .await
                .map_err(|error| error.to_string())?;
            Ok(())
        }
        Ok(response)
            if response.status == StatusCode::TOO_MANY_REQUESTS
                || response.status.is_server_error() =>
        {
            retry_job(
                state,
                job,
                message_id,
                format!("remote Chat returned {}", response.status),
            )
            .await
        }
        Ok(response) if response.status == StatusCode::FORBIDDEN => {
            clear_remote_grant(state, job, &contact.subject_id).await;
            finish_message(
                state,
                job,
                message_id,
                "blocked",
                "remote Chat delivery is not granted",
            )
            .await
        }
        Ok(response) => {
            finish_message(
                state,
                job,
                message_id,
                "failed",
                &format!("remote Chat returned {}", response.status),
            )
            .await
        }
        Err(error) if error == REMOTE_IDENTITY_CHANGED => {
            clear_remote_grant(state, job, &contact.subject_id).await;
            finish_message(state, job, message_id, "blocked", REMOTE_IDENTITY_CHANGED).await
        }
        Err(error) => retry_job(state, job, message_id, error).await,
    }
}

async fn clear_remote_grant(state: &Chat, job: &Job, subject_id: &access_control::SubjectId) {
    if let Err(error) = state
        .update_remote_chat_grant(&job.contact_name, subject_id, &Default::default())
        .await
    {
        tracing::warn!(contact = %job.contact_name, %error, "Chat remote grant cache could not be cleared");
    }
}

async fn mark_sending(state: &Chat, job: &Job, message_id: i64) -> Result<(), String> {
    let now = super::service::now().map_err(|error| error.1.to_owned())?;
    state.store.db().execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE chat_messages SET state = 'sending', attempt_count = attempt_count + 1, last_attempt_at = ?, updated_at = ? WHERE id = ?",
        [now.into(), now.into(), message_id.into()],
    )).await.map_err(|error| error.to_string())?;
    let _ = job;
    Ok(())
}

async fn retry_job(state: &Chat, job: &Job, message_id: i64, error: String) -> Result<(), String> {
    let now = super::service::now().map_err(|error| error.1.to_owned())?;
    let delay = backoff(job.attempt_count);
    let available = now + delay;
    let transaction = state
        .store
        .db()
        .begin()
        .await
        .map_err(|error| error.to_string())?;
    transaction.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE chat_messages SET state = 'queued', next_attempt_at = ?, updated_at = ?, error_message = ? WHERE id = ? AND EXISTS (SELECT 1 FROM chat_jobs WHERE id = ? AND lease_token = ?)",
        [available.into(), now.into(), error.as_str().into(), message_id.into(), job.id.into(), job.lease_token.as_str().into()],
    )).await.map_err(|error| error.to_string())?;
    transaction.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE chat_jobs SET state = 'queued', available_at = ?, attempt_count = attempt_count + 1, lease_token = NULL, lease_until = NULL, last_error = ?, updated_at = ? WHERE id = ? AND lease_token = ?",
        [available.into(), error.as_str().into(), now.into(), job.id.into(), job.lease_token.as_str().into()],
    )).await.map_err(|error| error.to_string())?;
    transaction
        .commit()
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

async fn finish_message(
    state: &Chat,
    job: &Job,
    message_id: i64,
    state_name: &str,
    error: &str,
) -> Result<(), String> {
    let now = super::service::now().map_err(|error| error.1.to_owned())?;
    let transaction = state
        .store
        .db()
        .begin()
        .await
        .map_err(|error| error.to_string())?;
    transaction.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE chat_messages SET state = ?, updated_at = ?, error_message = ?, next_attempt_at = NULL WHERE id = ? AND EXISTS (SELECT 1 FROM chat_jobs WHERE id = ? AND lease_token = ?)",
        [state_name.into(), now.into(), error.into(), message_id.into(), job.id.into(), job.lease_token.as_str().into()],
    )).await.map_err(|error| error.to_string())?;
    transaction.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE chat_jobs SET state = 'dead', lease_token = NULL, lease_until = NULL, last_error = ?, updated_at = ? WHERE id = ? AND lease_token = ?",
        [error.into(), now.into(), job.id.into(), job.lease_token.as_str().into()],
    )).await.map_err(|error| error.to_string())?;
    transaction
        .commit()
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn backoff(attempt_count: i64) -> i64 {
    let exponent = attempt_count.clamp(0, 10) as u32;
    (5_i64.saturating_mul(2_i64.saturating_pow(exponent))).min(MAX_BACKOFF_SECONDS)
}
