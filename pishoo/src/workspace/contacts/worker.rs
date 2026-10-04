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
use crate::workspace::Workspace;

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
    state: Weak<Workspace>,
    notify: Arc<Notify>,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // Incoming applications have no outbound queue row. Reuse this worker to
        // observe their reverse grants, including after a restart or revocation.
        let mut incoming = tokio::time::interval(Duration::from_secs(30));
        incoming.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            if shutdown.is_cancelled() {
                break;
            }
            let Some(workspace) = state.upgrade() else {
                break;
            };
            match claim(&workspace).await {
                Ok(Some(job)) => {
                    process(&workspace, job).await;
                    continue;
                }
                Ok(None) => {}
                Err(error) => tracing::warn!(%error, "contact worker could not claim request"),
            }
            drop(workspace);
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = notify.notified() => {},
                _ = incoming.tick() => {},
                _ = tokio::time::sleep(TICK) => continue,
            }
            let Some(state) = state.upgrade() else {
                break;
            };
            if let Err(error) = sync_incoming_grants(&state).await {
                tracing::warn!(%error, "contact worker could not confirm incoming Chat grants");
            }
        }
    })
}

async fn sync_incoming_grants(state: &Workspace) -> Result<(), String> {
    let Some(transport) = state.outbound.read().await.clone() else {
        return Ok(());
    };
    if transport
        .sender_subject_id()?
        .as_deref()
        .is_some_and(|subject| subject != state.owner.subject_id().as_bytes())
    {
        return Err("local outbound identity changed".into());
    }
    let contacts = state
        .access
        .database()
        .query_all_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT c.id, c.name, c.subject_id FROM contacts c WHERE c.status = 2 \
         AND (json_extract(c.requests, '$.\"/std/message\"') IS NOT NULL \
           OR json_extract(c.offers, '$.\"/std/message\"') IS NOT NULL) \
         AND EXISTS (SELECT 1 FROM contact_applications a \
           WHERE a.applicant_name = c.name AND a.applicant_sid = c.subject_id)"
                .to_owned(),
        ))
        .await
        .map_err(|error| error.to_string())?;
    for row in contacts {
        let id: i64 = row.try_get("", "id").map_err(|error| error.to_string())?;
        let name: String = row.try_get("", "name").map_err(|error| error.to_string())?;
        let subject: Vec<u8> = row
            .try_get("", "subject_id")
            .map_err(|error| error.to_string())?;
        let result = async {
            let response = transport.request(&name, Method::GET, "/contact/self", Bytes::new()).await?;
            if response.remote_subject_id != subject {
                return Err("remote contact identity changed".to_owned());
            }
            let granted = match response.status {
                StatusCode::OK => {
                    // This endpoint describes the authenticated caller, unlike
                    // the application-id response which describes the server.
                    let body: serde_json::Value = serde_json::from_slice(&response.body)
                        .map_err(|error| error.to_string())?;
                    if body["name"].as_str() != Some(state.owner.name())
                        || body["subject_id"].as_str().map(str::as_bytes)
                            != Some(state.owner.subject_id().as_bytes())
                    {
                        return Err("remote status belongs to a different local identity".to_owned());
                    }
                    let status: access_control::ContactStatus = serde_json::from_value(body["status"].clone())
                        .map_err(|error| error.to_string())?;
                    let grants: access_control::GrantedAccess = serde_json::from_value(body["granted_access"].clone())
                        .map_err(|error| error.to_string())?;
                    if status == access_control::ContactStatus::Active {
                        grants
                    } else {
                        Default::default()
                    }
                }
                StatusCode::NOT_FOUND => {
                    // Immediately after approval the applicant may not have
                    // polled our decision and created its reverse record yet.
                    // Absence is not a denial of a grant we never observed.
                    let chat = state.chat.read().await.as_ref().and_then(Weak::upgrade)
                        .ok_or_else(|| "Chat module unavailable".to_owned())?;
                    let observed = chat.remote_chat_grants().await.map_err(|error| error.to_string())?
                        .into_iter().any(|(peer, identity, _)| peer == name && identity == subject);
                    if !observed { return Ok(()); }
                    Default::default()
                }
                StatusCode::FORBIDDEN => Default::default(),
                status => return Err(format!("remote contact status returned {status}")),
            };
            let _write = state.contact_write.lock().await;
            // Do not attach an in-flight response to a deleted/replaced contact.
            let current = state.access.database().query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT 1 FROM contacts WHERE id = ? AND name = ? AND subject_id = ? AND status = 2",
                [id.into(), name.as_str().into(), subject.clone().into()],
            )).await.map_err(|error| error.to_string())?;
            if current.is_some() {
                let subject = access_control::SubjectId::new(subject).map_err(|_| "invalid contact identity".to_owned())?;
                state.update_remote_chat_grant(&name, &subject, &granted).await
                    .map_err(|error| error.to_string())?;
            }
            Ok::<_, String>(())
        }.await;
        if let Err(error) = result {
            tracing::warn!(%error, contact = %name, "incoming Chat grant confirmation failed; will retry");
        }
    }
    Ok(())
}

async fn claim(state: &Workspace) -> Result<Option<Job>, String> {
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

async fn process(state: &Workspace, job: Job) {
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

async fn finish_response(state: &Workspace, job: &Job, remote: RemoteStatus, current: i64) {
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

async fn finish_error(state: &Workspace, job: &Job, transient: bool, reason: &str, current: i64) {
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

#[cfg(test)]
mod tests {
    use access_control::{AccessService, SubjectId, Visitor};
    use axum::body::Body;
    use futures::future::BoxFuture;
    use tower::ServiceExt;

    use super::*;
    use crate::{
        chat::{Chat, store::ChatStore},
        workspace::{
            outbound::{OutboundTransport, RemoteResponse},
            store::WorkspaceStore,
        },
    };

    struct Reply {
        status: StatusCode,
        body: serde_json::Value,
        subject: Vec<u8>,
    }

    impl OutboundTransport for Reply {
        fn sender_subject_id(&self) -> Result<Option<Vec<u8>>, String> {
            Ok(Some(b"receiver-key".to_vec()))
        }

        fn request<'a>(
            &'a self,
            target: &'a str,
            method: Method,
            path: &'a str,
            body: Bytes,
        ) -> BoxFuture<'a, Result<RemoteResponse, String>> {
            Box::pin(async move {
                assert_eq!(target, "alice.dhttp.net");
                assert_eq!(method, Method::GET);
                assert_eq!(path, "/contact/self");
                assert!(body.is_empty());
                Ok(RemoteResponse {
                    status: self.status,
                    headers: Default::default(),
                    body: Bytes::from(serde_json::to_vec(&self.body).unwrap()),
                    remote_subject_id: self.subject.clone(),
                })
            })
        }
    }

    async fn fixture() -> (tempfile::TempDir, Arc<Workspace>, Arc<Chat>) {
        let root = tempfile::tempdir().unwrap();
        let profile =
            dhttp_home::identity::IdentityProfile::try_from(root.path().join("receiver")).unwrap();
        let store = WorkspaceStore::open(&profile).await.unwrap();
        let subject = SubjectId::new(b"receiver-key").unwrap();
        let access = Arc::new(
            AccessService::load_from_db(
                &format!("sqlite://{}?mode=rwc", profile.access_db_path().display()),
                "receiver.dhttp.net",
                &subject,
            )
            .await
            .unwrap(),
        );
        // Enter through the real daccess application route, including offers.
        let mut request = http::Request::builder()
            .method(Method::POST)
            .uri("/contact")
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({
                    "application_id": "a".repeat(64),
                    "description": "Alice",
                    "requested_access": {"/std/message": ["POST"]},
                    "offers": {"/std/message": {"allow": ["POST"], "review": [], "deny": []}}
                })
                .to_string(),
            ))
            .unwrap();
        request.extensions_mut().insert(Visitor::new(
            "alice.dhttp.net",
            SubjectId::new(b"alice-key").unwrap(),
        ));
        let response = access_control::management_router(access.clone())
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let chat = Arc::new(Chat::new(
            "receiver.dhttp.net".into(),
            subject.clone(),
            ChatStore::open(&profile).await.unwrap(),
            access.clone(),
        ));
        let state = Arc::new(Workspace::new(
            "receiver".into(),
            "receiver.dhttp.net".into(),
            subject,
            store,
            access,
        ));
        state.configure_chat(chat.clone()).await;
        (root, state, chat)
    }

    fn granted() -> serde_json::Value {
        serde_json::json!({
            "name": "receiver.dhttp.net", "subject_id": "receiver-key", "status": "active",
            "granted_access": {"/std/message": {"allow": ["POST"], "review": [], "deny": []}}
        })
    }

    async fn confirm(
        state: &Workspace,
        status: StatusCode,
        body: serde_json::Value,
        subject: &[u8],
    ) {
        state
            .configure_outbound(Arc::new(Reply {
                status,
                body,
                subject: subject.to_vec(),
            }))
            .await;
        sync_incoming_grants(state).await.unwrap();
    }

    #[tokio::test]
    async fn incoming_offer_requires_verified_confirmation_and_tracks_revocation() {
        let (_root, state, chat) = fixture().await;
        confirm(&state, StatusCode::OK, granted(), b"alice-key").await;
        assert!(
            chat.remote_chat_grants().await.unwrap().is_empty(),
            "pending contacts are not confirmed"
        );
        super::super::grant_capability(&state, "alice.dhttp.net", "chat", None)
            .await
            .unwrap();
        assert!(
            chat.remote_chat_grants().await.unwrap().is_empty(),
            "an offer alone is not confirmation"
        );
        confirm(
            &state,
            StatusCode::NOT_FOUND,
            serde_json::Value::Null,
            b"alice-key",
        )
        .await;
        assert!(chat.remote_chat_grants().await.unwrap().is_empty(),
            "first 404 means the sender may not have reconciled yet, not a denial");
        confirm(&state, StatusCode::OK, granted(), b"alice-key").await;
        assert!(chat.remote_chat_grants().await.unwrap()[0].2);
        confirm(&state, StatusCode::NOT_FOUND, serde_json::Value::Null, b"alice-key").await;
        assert!(!chat.remote_chat_grants().await.unwrap()[0].2, "removal after confirmation revokes the observed grant");
        confirm(&state, StatusCode::OK, granted(), b"alice-key").await;
        confirm(
            &state,
            StatusCode::SERVICE_UNAVAILABLE,
            serde_json::Value::Null,
            b"alice-key",
        )
        .await;
        assert!(
            chat.remote_chat_grants().await.unwrap()[0].2,
            "transient errors preserve the last observation"
        );
        let mut revoked = granted();
        revoked["granted_access"] = serde_json::json!({});
        confirm(&state, StatusCode::OK, revoked, b"alice-key").await;
        assert!(!chat.remote_chat_grants().await.unwrap()[0].2);
        confirm(&state, StatusCode::OK, granted(), b"alice-key").await;
        let mut blocked = granted();
        blocked["status"] = serde_json::json!("blocked");
        confirm(&state, StatusCode::OK, blocked, b"alice-key").await;
        assert!(
            !chat.remote_chat_grants().await.unwrap()[0].2,
            "inactive peers cannot grant permission"
        );
    }

    #[tokio::test]
    async fn incoming_confirmation_rejects_wrong_peer_caller_and_malformed_response() {
        let (_root, state, chat) = fixture().await;
        super::super::grant_capability(&state, "alice.dhttp.net", "chat", None)
            .await
            .unwrap();
        confirm(&state, StatusCode::OK, granted(), b"replacement-key").await;
        assert!(chat.remote_chat_grants().await.unwrap().is_empty());
        for (key, value) in [
            ("name", serde_json::json!("someone.dhttp.net")),
            ("subject_id", serde_json::json!("old-receiver-key")),
            ("status", serde_json::json!("invalid")),
            ("granted_access", serde_json::Value::Null),
        ] {
            let mut body = granted();
            body[key] = value;
            confirm(&state, StatusCode::OK, body, b"alice-key").await;
            assert!(
                chat.remote_chat_grants().await.unwrap().is_empty(),
                "accepted invalid {key}"
            );
        }
        confirm(&state, StatusCode::OK, granted(), b"alice-key").await;
        assert!(chat.remote_chat_grants().await.unwrap()[0].2);
    }
}
