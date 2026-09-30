use std::sync::{Arc, Mutex as StdMutex};

use access_control::{AccessService, GrantedAccess, SubjectId, Visitor};
use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, State},
    routing::get,
};
use http::StatusCode;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement, TransactionTrait};
use serde::Serialize;
use tokio::{
    sync::{Notify, RwLock},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use self::{actor::Owner, store::ChatStore};

mod actor;
mod bridge;
pub(crate) mod capabilities;
mod message;
mod messages;
pub(crate) mod outbound;
mod service;
pub(crate) mod store;
mod worker;

pub(crate) const CHAT_CAPABILITY: capabilities::ChatCapability = capabilities::ChatCapability;

pub(crate) struct ChatState {
    owner: Owner,
    store: ChatStore,
    access: Arc<AccessService>,
    outbound: RwLock<Option<Arc<dyn outbound::OutboundTransport>>>,
    worker_notify: Arc<Notify>,
    worker_shutdown: CancellationToken,
    worker_handle: StdMutex<Option<JoinHandle<()>>>,
}

impl ChatState {
    pub(crate) fn new(
        owner_name: String,
        owner_subject_id: SubjectId,
        store: ChatStore,
        access: Arc<AccessService>,
    ) -> Self {
        Self {
            owner: Owner::new(owner_name, owner_subject_id),
            store,
            access,
            outbound: RwLock::new(None),
            worker_notify: Arc::new(Notify::new()),
            worker_shutdown: CancellationToken::new(),
            worker_handle: StdMutex::new(None),
        }
    }

    pub(crate) fn start_worker(self: &Arc<Self>) {
        let mut handle = self.worker_handle.lock().expect("Chat worker handle lock");
        if handle.is_none() {
            *handle = Some(worker::spawn(
                Arc::downgrade(self),
                self.worker_notify.clone(),
                self.worker_shutdown.child_token(),
            ));
        }
    }

    pub(crate) fn wake_worker(&self) {
        self.worker_notify.notify_one();
    }

    pub(crate) async fn remote_chat_grants(
        &self,
    ) -> Result<Vec<(String, Vec<u8>, bool)>, sea_orm::DbErr> {
        self.store
            .db()
            .query_all_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT contact_name, subject_id, remote_message_granted FROM chat_capability_state \
                 WHERE subject_id IS NOT NULL"
                    .to_owned(),
            ))
            .await?
            .into_iter()
            .map(|row| {
                Ok((
                    row.try_get("", "contact_name")?,
                    row.try_get("", "subject_id")?,
                    row.try_get::<i64>("", "remote_message_granted")? == 1,
                ))
            })
            .collect()
    }

    pub(crate) async fn configure_outbound(&self, outbound: Arc<dyn outbound::OutboundTransport>) {
        *self.outbound.write().await = Some(outbound);
        self.wake_worker();
    }

    pub(crate) async fn update_remote_chat_grant(
        &self,
        contact_name: &str,
        subject_id: &SubjectId,
        granted_access: &GrantedAccess,
    ) -> Result<(), sea_orm::DbErr> {
        let contact = self.access.find_contact_by_name(contact_name).await?;
        if contact.subject_id != *subject_id {
            return Err(sea_orm::DbErr::Type(String::from(
                "remote Chat grant does not match the contact subject_id",
            )));
        }
        let granted = granted_access
            .get("/std/message")
            .is_some_and(|methods| {
                methods
                .allow
                .iter()
                .any(|method| method == &access_control::Method::Specified(http::Method::POST))
            });
        let updated_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| sea_orm::DbErr::Type(String::from("invalid system clock")))?
            .as_secs() as i64;
        let transaction = self.store.db().begin().await?;
        transaction
            .execute_raw(sea_orm::Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Sqlite,
                "INSERT INTO chat_capability_state (contact_name, subject_id, remote_message_granted, updated_at) VALUES (?, ?, ?, ?) \
                 ON CONFLICT(contact_name) DO UPDATE SET subject_id = excluded.subject_id, remote_message_granted = excluded.remote_message_granted, updated_at = excluded.updated_at",
                [
                    contact_name.into(),
                    subject_id.as_bytes().to_vec().into(),
                    (if granted { 1_i64 } else { 0_i64 }).into(),
                    updated_at.into(),
                ],
            ))
            .await?;
        if granted {
            transaction
                .execute_raw(sea_orm::Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Sqlite,
                    "UPDATE chat_messages SET state = 'queued', next_attempt_at = ?, error_message = NULL, updated_at = ? \
                     WHERE contact_name = ? AND recipient_subject_id = ? AND direction = 'outgoing' AND state = 'blocked' \
                       AND error_message = 'remote Chat delivery is not granted'",
                    [updated_at.into(), updated_at.into(), contact_name.into(), subject_id.as_bytes().to_vec().into()],
                ))
                .await?;
            transaction
                .execute_raw(sea_orm::Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Sqlite,
                    "UPDATE chat_jobs SET state = 'queued', available_at = ?, lease_token = NULL, lease_until = NULL, last_error = NULL, updated_at = ? \
                     WHERE kind = 'send' AND contact_name = ? AND state = 'dead' \
                       AND message_id IN (SELECT id FROM chat_messages WHERE contact_name = ? AND recipient_subject_id = ? AND state = 'queued')",
                    [
                        updated_at.into(),
                        updated_at.into(),
                        contact_name.into(),
                        contact_name.into(),
                        subject_id.as_bytes().to_vec().into(),
                    ],
                ))
                .await?;
        }
        transaction.commit().await?;
        self.wake_worker();
        Ok(())
    }
}

impl Drop for ChatState {
    fn drop(&mut self) {
        self.worker_shutdown.cancel();
    }
}

#[derive(Serialize)]
struct ChatContext {
    owner_name: String,
    capability: &'static str,
    endpoints: &'static [capabilities::CapabilityEndpoint],
}

async fn context(
    State(state): State<Arc<ChatState>>,
    visitor: Option<Extension<Visitor>>,
) -> Result<Json<ChatContext>, StatusCode> {
    state
        .owner
        .ensure(visitor.as_ref().map(|extension| &extension.0))?;
    Ok(Json(ChatContext {
        owner_name: state.owner.name().to_owned(),
        capability: CHAT_CAPABILITY.id(),
        endpoints: CHAT_CAPABILITY.endpoints(),
    }))
}

pub(crate) fn router(state: Arc<ChatState>) -> Router {
    Router::new()
        .route(
            "/std/message",
            axum::routing::post(messages::post)
                .layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route("/chat-api/context", get(context))
        .route(
            "/chat-api/conversations/{name}/messages",
            get(bridge::get_messages)
                .post(bridge::post_message)
                .layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route(
            "/chat-api/conversations/{name}/messages/{id}/requeue",
            axum::routing::post(bridge::requeue_message),
        )
        .route(
            "/chat-api/conversations/{name}/capability",
            get(bridge::get_capability),
        )
        .with_state(state)
}
