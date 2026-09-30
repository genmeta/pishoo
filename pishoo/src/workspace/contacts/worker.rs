use std::{
    sync::{Arc, Weak},
    time::Duration,
};

use bytes::Bytes;
use http::{Method, StatusCode};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement, TransactionTrait};
use tokio::{sync::Notify, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use super::{
    RemoteApplication, RemoteStatus, now, reconcile_remote_contact, selected_access,
    sync_remote_chat_grant,
};
use crate::workspace::WorkspaceState;

const TICK: Duration = Duration::from_secs(3);
const LEASE_SECONDS: i64 = 120;

struct Job {
    id: i64,
    target: String,
    application_id: String,
    sender_subject_id: Vec<u8>,
    recipient_subject_id: Option<Vec<u8>>,
    description: String,
    requested_capabilities: Vec<String>,
    offered_capabilities: Vec<String>,
    status: String,
    expired_after: i64,
    attempt_count: i64,
    lease_until: i64,
}

pub(crate) fn spawn(
    state: Weak<WorkspaceState>,
    notify: Arc<Notify>,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if shutdown.is_cancelled() {
                break;
            }
            let Some(state) = state.upgrade() else {
                break;
            };
            match claim(&state).await {
                Ok(Some(job)) => {
                    process(&state, job).await;
                    continue;
                }
                Ok(None) => {}
                Err(error) => tracing::warn!(%error, "contact worker could not claim request"),
            }
            drop(state);
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = notify.notified() => {},
                _ = tokio::time::sleep(TICK) => {},
            }
        }
    })
}

async fn claim(state: &WorkspaceState) -> Result<Option<Job>, String> {
    let current = now().map_err(|error| error.1.to_owned())?;
    let transaction = state
        .store
        .db()
        .begin()
        .await
        .map_err(|error| error.to_string())?;
    transaction.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE outbound_contact_requests SET status = CASE WHEN status = 'queued' THEN 'failed' ELSE 'expired' END, \
         error_message = CASE WHEN status = 'queued' THEN '投递期限结束，未确认是否送达' ELSE error_message END, updated_at = ? \
         WHERE status IN ('queued', 'pending') AND expired_after <= ?",
        [current.into(), current.into()],
    )).await.map_err(|error| error.to_string())?;
    let row = transaction.query_one_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "SELECT id, target_name, application_id, sender_subject_id, recipient_subject_id, description, requested_capabilities, offered_capabilities, status, expired_after, attempt_count \
         FROM outbound_contact_requests WHERE application_id IS NOT NULL AND status IN ('queued', 'pending', 'active') \
         AND next_attempt_at <= ? AND (lease_until IS NULL OR lease_until <= ?) \
         ORDER BY next_attempt_at, id LIMIT 1",
        [current.into(), current.into()],
    )).await.map_err(|error| error.to_string())?;
    let Some(row) = row else {
        transaction
            .commit()
            .await
            .map_err(|error| error.to_string())?;
        return Ok(None);
    };
    let id: i64 = row.try_get("", "id").map_err(|error| error.to_string())?;
    let requested: String = row
        .try_get("", "requested_capabilities")
        .map_err(|error| error.to_string())?;
    let offered: String = row
        .try_get("", "offered_capabilities")
        .map_err(|error| error.to_string())?;
    let lease_until = current + LEASE_SECONDS;
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE outbound_contact_requests SET lease_until = ? WHERE id = ?",
            [lease_until.into(), id.into()],
        ))
        .await
        .map_err(|error| error.to_string())?;
    transaction
        .commit()
        .await
        .map_err(|error| error.to_string())?;
    Ok(Some(Job {
        id,
        target: row
            .try_get("", "target_name")
            .map_err(|error| error.to_string())?,
        application_id: row
            .try_get("", "application_id")
            .map_err(|error| error.to_string())?,
        sender_subject_id: row
            .try_get("", "sender_subject_id")
            .map_err(|error| error.to_string())?,
        recipient_subject_id: row
            .try_get("", "recipient_subject_id")
            .map_err(|error| error.to_string())?,
        description: row
            .try_get("", "description")
            .map_err(|error| error.to_string())?,
        requested_capabilities: serde_json::from_str(&requested)
            .map_err(|error| error.to_string())?,
        offered_capabilities: serde_json::from_str(&offered).map_err(|error| error.to_string())?,
        status: row
            .try_get("", "status")
            .map_err(|error| error.to_string())?,
        expired_after: row
            .try_get("", "expired_after")
            .map_err(|error| error.to_string())?,
        attempt_count: row
            .try_get("", "attempt_count")
            .map_err(|error| error.to_string())?,
        lease_until,
    }))
}

async fn process(state: &WorkspaceState, job: Job) {
    let current = match now() {
        Ok(value) => value,
        Err(_) => return,
    };
    let transport = state.outbound.read().await.clone();
    let outcome = if job.sender_subject_id.as_slice() != state.owner.subject_id().as_bytes() {
        Err((false, "本地身份已变更，请重新发送申请".to_owned()))
    } else if let Some(transport) = transport {
        match transport.sender_subject_id() {
            Err(error) => Err((true, error)),
            Ok(Some(subject_id)) if subject_id.as_slice() != job.sender_subject_id.as_slice() => {
                Err((false, "出站证书身份已变更，请重新发送申请".to_owned()))
            }
            _ => {
                let request = if job.status == "queued" {
                    let access =
                        selected_access(&job.requested_capabilities, &job.offered_capabilities);
                    match access {
                        Ok((requested_access, offers)) => serde_json::to_vec(&RemoteApplication {
                            application_id: &job.application_id,
                            class: "",
                            description: &job.description,
                            requested_access: &requested_access,
                            offers: &offers,
                        })
                        .map(|body| (Method::POST, "/contact".to_owned(), Bytes::from(body)))
                        .map_err(|error| error.to_string()),
                        Err((_, error)) => Err(error.to_owned()),
                    }
                } else {
                    Ok((
                        Method::GET,
                        format!("/contact/self?application_id={}", job.application_id),
                        Bytes::new(),
                    ))
                };
                match request {
                    Err(error) => Err((false, error)),
                    Ok((method, path, body)) => {
                        match transport.request(&job.target, method, &path, body).await {
                            Err(error) => Err((true, error)),
                            Ok(response)
                                if response.status == StatusCode::OK
                                    || response.status == StatusCode::CREATED =>
                            {
                                match serde_json::from_slice::<RemoteStatus>(&response.body) {
                                    Ok(remote)
                                        if remote.subject_id.as_bytes()
                                            == response.remote_subject_id.as_slice() =>
                                    {
                                        Ok(remote)
                                    }
                                    Ok(_) => Err((false, "远端响应身份与证书不一致".to_owned())),
                                    Err(error) => Err((false, format!("无效的远端响应：{error}"))),
                                }
                            }
                            Ok(response) => {
                                let transient = response.status.is_server_error()
                                    || matches!(
                                        response.status,
                                        StatusCode::TOO_MANY_REQUESTS | StatusCode::REQUEST_TIMEOUT
                                    );
                                Err((transient, format!("远端返回 {}", response.status)))
                            }
                        }
                    }
                }
            }
        }
    } else {
        Err((true, "等待本地网络连接".to_owned()))
    };
    match outcome {
        Ok(remote) => finish_response(state, &job, remote, current).await,
        Err((transient, reason)) => finish_error(state, &job, transient, &reason, current).await,
    }
}

async fn finish_response(state: &WorkspaceState, job: &Job, remote: RemoteStatus, current: i64) {
    let remote_lifetime = remote.expired_after.checked_sub(remote.received_at);
    if remote.application_id != job.application_id
        || remote.name != job.target
        || !remote_lifetime.is_some_and(|duration| (1..=7 * 86_400).contains(&duration))
    {
        finish_error(state, job, false, "远端申请状态不匹配", current).await;
        return;
    }
    if remote.subject_id.is_empty() {
        finish_error(state, job, false, "远端身份缺失", current).await;
        return;
    }
    if job
        .recipient_subject_id
        .as_deref()
        .is_some_and(|sid| sid != remote.subject_id.as_bytes())
    {
        finish_error(state, job, false, "远端身份已变更", current).await;
        return;
    }
    let result = match remote.status.as_str() {
        "active" => {
            reconcile_remote_contact(
                state,
                &job.target,
                &remote,
                &job.requested_capabilities,
                &job.offered_capabilities,
                current,
            )
            .await
        }
        "pending" | "expired" | "denied" | "revoked" => {
            sync_remote_chat_grant(state, &job.target, &remote).await
        }
        _ => Err((StatusCode::CONFLICT, "未知远端申请状态")),
    };
    if let Err((_, message)) = result {
        finish_error(state, job, false, message, current).await;
        return;
    }
    let next_attempt = if remote.status == "pending" {
        current + 30
    } else {
        current + 300
    };
    let next_attempt = if matches!(remote.status.as_str(), "pending" | "active") {
        next_attempt
    } else {
        i64::MAX
    };
    let status = remote.status;
    let local_expired_after = if job.status == "queued" {
        current.saturating_add(remote_lifetime.unwrap_or(0))
    } else {
        job.expired_after
    };
    if let Err(error) = state.store.db().execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE outbound_contact_requests SET status = ?, expired_after = ?, remote_expired_after = ?, recipient_subject_id = ?, \
         next_attempt_at = ?, attempt_count = 0, lease_until = NULL, error_message = NULL, \
         last_checked_at = ?, updated_at = ? WHERE id = ? AND lease_until = ? AND status = ?",
        [status.into(), local_expired_after.into(), remote.expired_after.into(), remote.subject_id.as_bytes().to_vec().into(), next_attempt.into(),
         current.into(), current.into(), job.id.into(), job.lease_until.into(), job.status.as_str().into()],
    )).await {
        tracing::warn!(%error, request_id = job.id, "contact worker could not save remote status");
    }
}

async fn finish_error(
    state: &WorkspaceState,
    job: &Job,
    transient: bool,
    reason: &str,
    current: i64,
) {
    let deadline = job.expired_after;
    let terminal = !transient || (job.status != "active" && current >= deadline);
    let reason = if transient && terminal && job.status == "queued" {
        "投递期限结束，未确认是否送达"
    } else {
        reason
    };
    let backoff = 5_i64
        .saturating_mul(
            1_i64
                .checked_shl((job.attempt_count as u32).min(9))
                .unwrap_or(512),
        )
        .min(1800);
    let status = if terminal {
        "failed"
    } else {
        job.status.as_str()
    };
    let next_attempt = if terminal {
        i64::MAX
    } else {
        current + backoff
    };
    if let Err(error) = state.store.db().execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE outbound_contact_requests SET status = ?, next_attempt_at = ?, attempt_count = attempt_count + 1, \
         lease_until = NULL, error_message = ?, last_checked_at = ?, updated_at = ? \
         WHERE id = ? AND lease_until = ? AND status = ?",
        [status.into(), next_attempt.into(), reason.into(), current.into(), current.into(), job.id.into(), job.lease_until.into(), job.status.as_str().into()],
    )).await {
        tracing::warn!(%error, request_id = job.id, "contact worker could not save delivery error");
    }
}
