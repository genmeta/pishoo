use std::sync::{Arc, Mutex as StdMutex, Weak};

use access_control::{AccessService, SubjectId, Visitor};
use axum::{Extension, Json, Router, extract::State, routing::get};
use http::StatusCode;
use serde::Serialize;
use tokio::{
    sync::{Notify, RwLock},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use self::{actor::Owner, outbound::OutboundTransport, store::WorkspaceStore};

mod actor;
mod approvals;
pub(crate) mod capabilities;
mod contacts;
pub(crate) mod directory;
pub(crate) mod outbound;
mod profile;
mod settings;
pub(crate) mod store;

pub struct Workspace {
    owner: Owner,
    profile: String,
    store: WorkspaceStore,
    access: Arc<AccessService>,
    outbound: RwLock<Option<Arc<dyn OutboundTransport>>>,
    chat: RwLock<Option<Weak<crate::chat::Chat>>>,
    outbound_send: tokio::sync::Mutex<()>,
    worker_notify: Arc<Notify>,
    worker_shutdown: CancellationToken,
    worker_handle: StdMutex<Option<JoinHandle<()>>>,
    profile_write: tokio::sync::Mutex<()>,
    contact_write: tokio::sync::Mutex<()>,
}

impl Workspace {
    pub(crate) fn new(
        profile: String,
        owner_name: String,
        owner_subject_id: SubjectId,
        store: WorkspaceStore,
        access: Arc<AccessService>,
    ) -> Self {
        Self {
            owner: Owner::new(owner_name, owner_subject_id),
            profile,
            store,
            access,
            outbound: RwLock::new(None),
            chat: RwLock::new(None),
            outbound_send: tokio::sync::Mutex::new(()),
            worker_notify: Arc::new(Notify::new()),
            worker_shutdown: CancellationToken::new(),
            worker_handle: StdMutex::new(None),
            profile_write: tokio::sync::Mutex::new(()),
            contact_write: tokio::sync::Mutex::new(()),
        }
    }

    pub(crate) async fn configure_outbound(&self, outbound: Arc<dyn OutboundTransport>) {
        *self.outbound.write().await = Some(outbound);
        self.wake_worker();
    }

    fn start_worker(self: &Arc<Self>) {
        let mut handle = self
            .worker_handle
            .lock()
            .expect("Workspace worker handle lock");
        if handle.is_none() {
            *handle = Some(contacts::worker::spawn(
                Arc::downgrade(self),
                self.worker_notify.clone(),
                self.worker_shutdown.child_token(),
            ));
        }
    }

    fn wake_worker(&self) {
        self.worker_notify.notify_one();
    }

    pub(crate) async fn shutdown(&self) {
        self.worker_shutdown.cancel();
        let handle = self
            .worker_handle
            .lock()
            .expect("Workspace worker handle lock")
            .take();
        if let Some(handle) = handle {
            handle.abort();
            let _ = handle.await;
        }
    }

    pub(crate) async fn configure_chat(&self, chat: Arc<crate::chat::Chat>) {
        *self.chat.write().await = Some(Arc::downgrade(&chat));
    }

    pub(crate) async fn update_remote_chat_grant(
        &self,
        contact_name: &str,
        subject_id: &SubjectId,
        granted_access: &access_control::GrantedAccess,
    ) -> Result<(), sea_orm::DbErr> {
        let chat = self.chat.read().await.as_ref().and_then(Weak::upgrade);
        if let Some(chat) = chat {
            chat.update_remote_chat_grant(contact_name, subject_id, granted_access)
                .await?;
        }
        Ok(())
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        self.worker_shutdown.cancel();
        if let Ok(mut handle) = self.worker_handle.lock()
            && let Some(handle) = handle.take()
        {
            handle.abort();
        }
    }
}

#[derive(Serialize)]
struct RuntimeContext {
    profile: String,
    owner_name: String,
    badges: BadgeCounts,
}

#[derive(Serialize)]
struct BadgeCounts {
    pending_reviews: u64,
    // Counts incoming contacts that carry at least one requested capability.
    incoming_contacts: Option<u64>,
}

async fn context(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<Visitor>>,
) -> Result<Json<RuntimeContext>, StatusCode> {
    state
        .owner
        .ensure(visitor.as_ref().map(|extension| &extension.0))?;
    let (pending_reviews, _) = state
        .access
        .pending_persistent_reviews(0, 1)
        .await
        .map_err(|error| {
            tracing::error!(error = %error, "Workspace pending review count failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let incoming_contacts = u64::try_from(
        contacts::pending_capability_requests(&state)
            .await
            .map_err(|(status, _)| status)?
            .len(),
    )
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(RuntimeContext {
        profile: state.profile.clone(),
        owner_name: state.owner.name().to_owned(),
        badges: BadgeCounts {
            pending_reviews,
            incoming_contacts: Some(incoming_contacts),
        },
    }))
}

pub(crate) fn router(state: Arc<Workspace>) -> Router {
    state.start_worker();
    Router::new()
        .route("/workspace-api/context", get(context))
        .route("/workspace-api/capabilities", get(capabilities::list))
        .route("/workspace-api/approvals", get(approvals::list))
        .route(
            "/workspace-api/approvals/{kind}/{id}",
            axum::routing::delete(approvals::delete),
        )
        .route("/workspace-api/contact-directory", get(directory::list))
        .route(
            "/workspace-api/capability-requests",
            get(contacts::capability_requests),
        )
        .route(
            "/workspace-api/settings/profile",
            get(settings::get_profile).patch(settings::patch_profile),
        )
        .route(
            "/workspace-api/settings/profile/avatar",
            get(profile::get_avatar)
                .put(profile::put_avatar)
                .delete(profile::delete_avatar),
        )
        .route("/std/profile", get(profile::get_public_profile))
        .route("/std/profile/avatar", get(profile::get_public_avatar))
        .route(
            "/workspace-api/profiles/{name}",
            get(profile::get_remote_profile),
        )
        .route(
            "/workspace-api/profiles/{name}/avatar",
            get(profile::get_remote_avatar),
        )
        .route(
            "/workspace-api/contact-requests",
            get(contacts::list).post(contacts::create),
        )
        .route(
            "/workspace-api/contacts/{name}/capabilities/{capability}/grant",
            axum::routing::post(contacts::grant),
        )
        .route(
            "/workspace-api/contacts/{name}/capabilities/{capability}/revoke",
            axum::routing::post(contacts::revoke),
        )
        .route(
            "/workspace-api/contacts/{name}/capabilities/{capability}/deny",
            axum::routing::post(contacts::deny),
        )
        .route(
            "/workspace-api/contacts/{name}/saved",
            axum::routing::put(directory::save).delete(directory::unsave),
        )
        .route(
            "/workspace-api/contact-requests/{id}",
            get(contacts::get).delete(contacts::delete),
        )
        .route(
            "/workspace-api/contact-requests/{id}/refresh",
            axum::routing::post(contacts::refresh),
        )
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
    };

    use access_control::{
        AccessService, ContactPatch, ContactStatus, Effect, Grantee, Method as AccessMethod,
        NewContact, SubjectId, Visitor,
    };
    use axum::{
        body::{Body, to_bytes},
        response::Response,
    };
    use bytes::Bytes;
    use dhttp_home::identity::IdentityProfile;
    use futures::future::BoxFuture;
    use http::{Method, Request, StatusCode, header};
    use sea_orm::{ConnectionTrait, Database, DatabaseBackend, Statement};
    use tower::ServiceExt;

    use super::{
        Workspace,
        outbound::{OutboundTransport, RemoteResponse},
        router,
        store::WorkspaceStore,
    };
    use crate::chat::{self, Chat, store::ChatStore};

    static NEXT_FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

    #[derive(Default)]
    struct FakeOutbound {
        requests: Mutex<Vec<(String, Method, String, serde_json::Value)>>,
        active: AtomicBool,
        fail: AtomicBool,
        hang_profiles: AtomicBool,
    }

    impl OutboundTransport for FakeOutbound {
        fn sender_subject_id(&self) -> Result<Option<Vec<u8>>, String> {
            Ok(None)
        }

        fn request<'a>(
            &'a self,
            target: &'a str,
            method: Method,
            path: &'a str,
            body: Bytes,
        ) -> BoxFuture<'a, Result<RemoteResponse, String>> {
            Box::pin(async move {
                if self.hang_profiles.load(Ordering::SeqCst)
                    && matches!(path, "/std/profile" | "/std/profile/avatar")
                {
                    futures::future::pending::<()>().await;
                }
                if self.fail.load(Ordering::SeqCst) {
                    return Err(String::from("mock timeout"));
                }
                let payload = if body.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::from_slice(&body).map_err(|error| error.to_string())?
                };
                let application_id = if method == Method::POST && path == "/contact" {
                    payload["application_id"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned()
                } else {
                    path.split("application_id=")
                        .nth(1)
                        .unwrap_or_default()
                        .to_owned()
                };
                self.requests.lock().expect("mock log").push((
                    target.to_owned(),
                    method.clone(),
                    path.to_owned(),
                    payload,
                ));
                if path == "/std/profile" {
                    Ok(RemoteResponse {
                        status: StatusCode::OK,
                        headers: http::HeaderMap::new(),
                        body: Bytes::from_static(
                            br#"{"display_name":"Remote friend","avatar_url":"/std/profile/avatar","updated_at":1700000000}"#,
                        ),
                        remote_subject_id: format!("{target}-key").into_bytes(),
                    })
                } else if path == "/std/profile/avatar" {
                    let mut headers = http::HeaderMap::new();
                    headers.insert(
                        http::header::CONTENT_TYPE,
                        http::HeaderValue::from_static("image/jpeg"),
                    );
                    Ok(RemoteResponse {
                        status: StatusCode::OK,
                        headers,
                        body: Bytes::from_static(include_bytes!(
                            "../../assets/pishoo/pishoo-icon.jpg"
                        )),
                        remote_subject_id: format!("{target}-key").into_bytes(),
                    })
                } else if method == Method::POST {
                    let received_at = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs();
                    let expired_after = received_at + 7 * 86_400;
                    let body = format!(
                        r#"{{"application_id":"{application_id}","status":"pending","name":"{target}","subject_id":"{target}-key","received_at":{received_at},"expired_after":{expired_after},"granted_access":{{}}}}"#,
                    );
                    Ok(RemoteResponse {
                        status: StatusCode::CREATED,
                        headers: http::HeaderMap::new(),
                        body: Bytes::from(body),
                        remote_subject_id: format!("{target}-key").into_bytes(),
                    })
                } else {
                    let received_at = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs();
                    let expired_after = received_at + 7 * 86_400;
                    let status = if self.active.load(Ordering::SeqCst) {
                        "active"
                    } else {
                        "pending"
                    };
                    let granted_access = if status == "active" {
                        r#"{"/std/message":{"allow":["POST"],"review":[],"deny":[]}}"#
                    } else {
                        "{}"
                    };
                    let body = format!(
                        r#"{{"application_id":"{application_id}","status":"{status}","name":"{target}","subject_id":"{target}-key","received_at":{received_at},"expired_after":{expired_after},"granted_access":{granted_access}}}"#,
                    );
                    Ok(RemoteResponse {
                        status: StatusCode::OK,
                        headers: http::HeaderMap::new(),
                        body: Bytes::from(body),
                        remote_subject_id: format!("{target}-key").into_bytes(),
                    })
                }
            })
        }
    }

    fn request_to(
        path: &str,
        method: Method,
        body: &str,
        visitor: Option<Visitor>,
    ) -> Request<Body> {
        let mut request = Request::builder()
            .uri(path)
            .method(method)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_owned()))
            .expect("valid request");
        if let Some(visitor) = visitor {
            request.extensions_mut().insert(visitor);
        }
        request
    }

    fn request(method: Method, body: &str, visitor: Option<Visitor>) -> Request<Body> {
        request_to("/workspace-api/settings/profile", method, body, visitor)
    }

    async fn response_json(response: Response) -> serde_json::Value {
        let body = to_bytes(response.into_body(), 16_384)
            .await
            .expect("JSON response");
        serde_json::from_slice(&body).expect("valid JSON")
    }

    async fn fixture_named_with_access(
        name: &str,
        transport: Option<Arc<dyn OutboundTransport>>,
    ) -> (axum::Router, PathBuf, Visitor, Arc<AccessService>) {
        let root = std::env::temp_dir().join(format!(
            "pishoo-workspace-api-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time is valid")
                .as_nanos(),
            NEXT_FIXTURE_ID.fetch_add(1, Ordering::Relaxed),
        ));
        let profile = IdentityProfile::try_from(root.join(name)).expect("valid profile");
        let store = WorkspaceStore::open(&profile)
            .await
            .expect("Workspace store");
        let subject = SubjectId::new(format!("{name}-key").into_bytes()).expect("valid subject");
        let access_uri = format!("sqlite://{}?mode=rwc", profile.access_db_path().display());
        let access = Arc::new(
            AccessService::load_from_db(&access_uri, name, &subject)
                .await
                .expect("isolated access service"),
        );
        let state = Arc::new(Workspace::new(
            name.to_owned(),
            name.to_owned(),
            subject.clone(),
            store,
            access.clone(),
        ));
        let chat_store = ChatStore::open(&profile).await.expect("Chat store");
        let chat_state = Arc::new(Chat::new(
            name.to_owned(),
            subject.clone(),
            chat_store,
            access.clone(),
        ));
        state.configure_chat(chat_state.clone()).await;
        chat_state.start_worker();
        if let Some(transport) = transport {
            state.configure_outbound(transport).await;
        }
        let app = router(state).merge(chat::router(chat_state));
        (app, root, Visitor::new(name, subject), access)
    }

    async fn fixture_named(
        name: &str,
        transport: Option<Arc<dyn OutboundTransport>>,
    ) -> (axum::Router, PathBuf, Visitor) {
        let (app, root, visitor, _) = fixture_named_with_access(name, transport).await;
        (app, root, visitor)
    }

    async fn fixture_with_transport(
        transport: Option<Arc<dyn OutboundTransport>>,
    ) -> (axum::Router, PathBuf, Visitor) {
        fixture_named("owner.example.dhttp.net", transport).await
    }

    async fn fixture_with_access() -> (axum::Router, PathBuf, Visitor, Arc<AccessService>) {
        fixture_named_with_access("owner.example.dhttp.net", None).await
    }

    async fn fixture() -> (axum::Router, PathBuf, Visitor) {
        fixture_named("owner.example", None).await
    }

    async fn exact_rule_effect(
        access: &AccessService,
        name: &str,
        method: &str,
        api: &str,
    ) -> Option<String> {
        rule_effect(access, 0, name, method, api).await
    }

    async fn rule_effect(
        access: &AccessService,
        grantee_type: i32,
        grantee: &str,
        method: &str,
        api: &str,
    ) -> Option<String> {
        access
            .database()
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT effect FROM access_rules WHERE grantee_type = ? AND grantee = ? AND method = ? AND api = ?",
                [
                    grantee_type.into(),
                    grantee.into(),
                    method.into(),
                    api.into(),
                ],
            ))
            .await
            .expect("read access rule")
            .map(|row| row.try_get("", "effect").expect("rule effect"))
    }

    async fn wait_outbound_status(app: &axum::Router, owner: &Visitor, id: i64, expected: &str) {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let response = app
                    .clone()
                    .oneshot(request_to(
                        &format!("/workspace-api/contact-requests/{id}"),
                        Method::GET,
                        "",
                        Some(owner.clone()),
                    ))
                    .await
                    .expect("read outbound request");
                let body = response_json(response).await;
                if body["status"] == expected {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("outbound status update");
    }

    #[tokio::test]
    async fn outbound_contact_requests_are_owner_only_validated_and_tracked() {
        let remote = Arc::new(FakeOutbound::default());
        remote.fail.store(true, Ordering::SeqCst);
        let (app, root, owner) = fixture_with_transport(Some(remote.clone())).await;
        let path = "/workspace-api/contact-requests";
        let body = r#"{"target_name":"friend.example","description":"Hello","requested_capabilities":["chat"],"offered_capabilities":["chat"]}"#;
        let denied = app
            .clone()
            .oneshot(request_to(path, Method::POST, body, None))
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
        let rejected = app
            .clone()
            .oneshot(request_to(
                path,
                Method::POST,
                r#"{"target_name":"friend.example","description":"Hello","expires_in_days":3}"#,
                Some(owner.clone()),
            ))
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let queued = app
            .clone()
            .oneshot(request_to(path, Method::POST, body, Some(owner.clone())))
            .await
            .unwrap();
        assert_eq!(queued.status(), StatusCode::ACCEPTED);
        let queued = response_json(queued).await;
        assert_eq!(queued["status"], "queued");
        assert_eq!(
            queued["expired_after"].as_i64().unwrap() - queued["created_at"].as_i64().unwrap(),
            7 * 86_400
        );
        let id = queued["id"].as_i64().unwrap();
        let conflict = app
            .clone()
            .oneshot(request_to(path, Method::POST, body, Some(owner.clone())))
            .await
            .unwrap();
        assert_eq!(conflict.status(), StatusCode::CONFLICT);

        remote.fail.store(false, Ordering::SeqCst);
        let refresh = app
            .clone()
            .oneshot(request_to(
                &format!("{path}/{id}/refresh"),
                Method::POST,
                "",
                Some(owner.clone()),
            ))
            .await
            .unwrap();
        assert_eq!(refresh.status(), StatusCode::OK);
        wait_outbound_status(&app, &owner, id, "pending").await;
        let log = remote.requests.lock().unwrap();
        let sent = log
            .iter()
            .find(|(_, method, path, _)| method == &Method::POST && path == "/contact")
            .unwrap();
        assert_eq!(sent.0, "friend.example.dhttp.net");
        assert!(
            sent.3["application_id"]
                .as_str()
                .is_some_and(|id| id.len() == 64)
        );
        assert!(sent.3.get("expired_after").is_none());
        assert!(sent.3.get("subject_id").is_none());
        drop(log);

        remote.active.store(true, Ordering::SeqCst);
        app.clone()
            .oneshot(request_to(
                &format!("{path}/{id}/refresh"),
                Method::POST,
                "",
                Some(owner.clone()),
            ))
            .await
            .unwrap();
        wait_outbound_status(&app, &owner, id, "active").await;
        let capability = app
            .clone()
            .oneshot(request_to(
                "/chat-api/conversations/friend.example.dhttp.net/capability",
                Method::GET,
                "",
                Some(owner.clone()),
            ))
            .await
            .unwrap();
        assert_eq!(response_json(capability).await["can_send"], true);

        let deleted = app
            .clone()
            .oneshot(request_to(
                &format!("{path}/{id}"),
                Method::DELETE,
                "",
                Some(owner),
            ))
            .await
            .unwrap();
        assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn granting_chat_adds_fixed_rules_without_replacing_existing_rules() {
        let (app, root, owner, access) = fixture_with_access().await;
        access
            .create_contact(NewContact {
                name: String::from("friend.example.dhttp.net"),
                subject_id: SubjectId::new(b"friend-subject".to_vec()).expect("subject"),
                class: String::from("Human"),
                description: String::from("Friend"),
                requested_access: crate::chat::CHAT_CAPABILITY.requested_access(),
                offers: crate::chat::CHAT_CAPABILITY.offers(),
                created_at: 1_700_000_000,
                updated_at: 1_700_000_000,
                expired_after: 2_000_000_000,
            })
            .await
            .expect("contact");
        let grantee = Grantee::One(String::from("friend.example.dhttp.net"));
        access
            .set_policy(
                AccessMethod::Specified(http::Method::GET),
                "/calendar",
                Effect::Allow,
                grantee.clone(),
            )
            .await
            .expect("existing exact rule");
        access
            .set_policy(
                AccessMethod::Specified(http::Method::GET),
                "/files",
                Effect::Allow,
                Grantee::Group {
                    title: String::from("member"),
                    issuer: String::from("example"),
                },
            )
            .await
            .expect("existing group rule");

        let response = app
            .clone()
            .oneshot(request_to(
                "/workspace-api/contacts/friend.example.dhttp.net/capabilities/chat/grant",
                http::Method::POST,
                "",
                Some(owner.clone()),
            ))
            .await
            .expect("grant response");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            exact_rule_effect(&access, "friend.example.dhttp.net", "GET", "/std/message").await,
            None
        );
        assert_eq!(
            exact_rule_effect(&access, "friend.example.dhttp.net", "POST", "/std/message").await,
            Some(String::from("allow"))
        );
        assert_eq!(
            exact_rule_effect(&access, "friend.example.dhttp.net", "GET", "/calendar").await,
            Some(String::from("allow"))
        );
        assert_eq!(
            rule_effect(&access, 1, "member@example", "GET", "/files").await,
            Some(String::from("allow"))
        );
        let response = app
            .clone()
            .oneshot(request_to(
                "/workspace-api/contacts/friend.example.dhttp.net/capabilities/chat/revoke",
                http::Method::POST,
                "",
                Some(owner),
            ))
            .await
            .expect("revoke response");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            exact_rule_effect(&access, "friend.example.dhttp.net", "POST", "/std/message").await,
            None
        );
        drop(access);
        drop(app);
        std::fs::remove_dir_all(root).expect("remove isolated profile");
    }

    #[tokio::test]
    async fn granting_unrequested_chat_is_rejected() {
        let (app, root, owner, access) = fixture_with_access().await;
        access
            .create_contact(NewContact {
                name: String::from("profile-only.example.dhttp.net"),
                subject_id: SubjectId::new(b"profile-only-subject".to_vec()).expect("subject"),
                class: String::from("Human"),
                description: String::from("Profile only"),
                requested_access: Default::default(),
                offers: Default::default(),
                created_at: 1_700_000_000,
                updated_at: 1_700_000_000,
                expired_after: 2_000_000_000,
            })
            .await
            .expect("contact");
        let response = app
            .clone()
            .oneshot(request_to(
                "/workspace-api/contacts/profile-only.example.dhttp.net/capabilities/chat/grant",
                Method::POST,
                "",
                Some(owner),
            ))
            .await
            .expect("grant response");
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            exact_rule_effect(
                &access,
                "profile-only.example.dhttp.net",
                "POST",
                "/std/message",
            )
            .await,
            None
        );
        drop((app, access));
        std::fs::remove_dir_all(root).expect("remove isolated profile");
    }

    #[tokio::test]
    async fn directory_requires_effective_chat_or_an_explicit_local_record() {
        let (app, root, owner, access) = fixture_with_access().await;
        let name = "directory.example.dhttp.net";
        let subject = SubjectId::new(b"directory-subject".to_vec()).expect("subject");
        access
            .create_contact(NewContact {
                name: name.to_owned(),
                subject_id: subject.clone(),
                class: String::from("Human"),
                description: String::new(),
                requested_access: Default::default(),
                offers: Default::default(),
                created_at: 1_700_000_000,
                updated_at: 1_700_000_000,
                expired_after: 2_000_000_000,
            })
            .await
            .expect("contact");
        access
            .patch_contact(
                name,
                ContactPatch::LocalUpdate {
                    status: Some(ContactStatus::Active),
                    alias: None,
                },
            )
            .await
            .expect("active contact");
        let path = "/workspace-api/contact-directory";
        let list = app
            .clone()
            .oneshot(request_to(path, Method::GET, "", Some(owner.clone())))
            .await
            .expect("directory");
        assert_eq!(response_json(list).await, serde_json::json!([]));
        let save_path = format!("/workspace-api/contacts/{name}/saved");
        let forbidden = app
            .clone()
            .oneshot(request_to(&save_path, Method::PUT, "", None))
            .await
            .expect("unauthorized save");
        assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
        let saved = app
            .clone()
            .oneshot(request_to(&save_path, Method::PUT, "", Some(owner.clone())))
            .await
            .expect("save identity");
        assert_eq!(saved.status(), StatusCode::NO_CONTENT);
        let list = app
            .clone()
            .oneshot(request_to(path, Method::GET, "", Some(owner.clone())))
            .await
            .expect("saved directory");
        assert_eq!(
            response_json(list).await,
            serde_json::json!([{ "name": name, "saved": true, "chat_available": false, "remote_chat_granted": null }])
        );
        assert_eq!(
            exact_rule_effect(&access, name, "POST", "/std/message").await,
            None
        );
        let unsaved = app
            .clone()
            .oneshot(request_to(
                &save_path,
                Method::DELETE,
                "",
                Some(owner.clone()),
            ))
            .await
            .expect("remove saved identity");
        assert_eq!(unsaved.status(), StatusCode::NO_CONTENT);
        let list = app
            .clone()
            .oneshot(request_to(path, Method::GET, "", Some(owner.clone())))
            .await
            .expect("unsaved directory");
        assert_eq!(response_json(list).await, serde_json::json!([]));
        let saved = app
            .clone()
            .oneshot(request_to(&save_path, Method::PUT, "", Some(owner.clone())))
            .await
            .expect("save again");
        assert_eq!(saved.status(), StatusCode::NO_CONTENT);

        access
            .delete_contacts(&[name.to_owned()])
            .await
            .expect("delete original contact");
        access
            .create_contact(NewContact {
                name: name.to_owned(),
                subject_id: subject.clone(),
                class: String::from("Human"),
                description: String::new(),
                requested_access: crate::chat::CHAT_CAPABILITY.requested_access(),
                offers: Default::default(),
                created_at: 1_700_000_001,
                updated_at: 1_700_000_001,
                expired_after: 2_000_000_000,
            })
            .await
            .expect("new contact record");
        access
            .patch_contact(
                name,
                ContactPatch::LocalUpdate {
                    status: Some(ContactStatus::Active),
                    alias: None,
                },
            )
            .await
            .expect("activate replacement");
        let list = app
            .clone()
            .oneshot(request_to(path, Method::GET, "", Some(owner.clone())))
            .await
            .expect("replacement directory");
        assert_eq!(response_json(list).await, serde_json::json!([]));

        access
            .set_policy(
                AccessMethod::Specified(Method::POST),
                "/std/message",
                Effect::Allow,
                Grantee::One(name.to_owned()),
            )
            .await
            .expect("stale name-based Chat rule");
        let list = app
            .clone()
            .oneshot(request_to(path, Method::GET, "", Some(owner.clone())))
            .await
            .expect("directory without bound approval");
        assert_eq!(response_json(list).await, serde_json::json!([]));

        let granted = app
            .clone()
            .oneshot(request_to(
                &format!("/workspace-api/contacts/{name}/capabilities/chat/grant"),
                Method::POST,
                "",
                Some(owner.clone()),
            ))
            .await
            .expect("grant local Chat");
        assert_eq!(granted.status(), StatusCode::NO_CONTENT);
        let list = app
            .clone()
            .oneshot(request_to(path, Method::GET, "", Some(owner.clone())))
            .await
            .expect("effective directory");
        assert_eq!(
            response_json(list).await,
            serde_json::json!([{ "name": name, "saved": false, "chat_available": true, "remote_chat_granted": null }])
        );

        // daccess updates an active contact's subject in place; the old rule
        // must not make that replacement identity effective.
        access
            .create_contact(NewContact {
                name: name.to_owned(),
                subject_id: SubjectId::new(b"rotated-subject".to_vec()).expect("rotated subject"),
                class: String::from("Human"),
                description: String::new(),
                requested_access: crate::chat::CHAT_CAPABILITY.requested_access(),
                offers: Default::default(),
                created_at: 1_700_000_002,
                updated_at: 1_700_000_002,
                expired_after: 2_000_000_000,
            })
            .await
            .expect("rotate contact subject");
        let list = app
            .clone()
            .oneshot(request_to(path, Method::GET, "", Some(owner.clone())))
            .await
            .expect("directory after subject rotation");
        assert_eq!(response_json(list).await, serde_json::json!([]));
        let revoked = app
            .clone()
            .oneshot(request_to(
                &format!("/workspace-api/contacts/{name}/capabilities/chat/revoke"),
                Method::POST,
                "",
                Some(owner.clone()),
            ))
            .await
            .expect("revoke local Chat");
        assert_eq!(revoked.status(), StatusCode::NO_CONTENT);
        let list = app
            .clone()
            .oneshot(request_to(path, Method::GET, "", Some(owner.clone())))
            .await
            .expect("revoked directory");
        assert_eq!(response_json(list).await, serde_json::json!([]));
        access
            .patch_contact(
                name,
                ContactPatch::LocalUpdate {
                    status: Some(ContactStatus::Blocked),
                    alias: None,
                },
            )
            .await
            .expect("block contact");
        let list = app
            .clone()
            .oneshot(request_to(path, Method::GET, "", Some(owner)))
            .await
            .expect("blocked directory");
        assert_eq!(
            response_json(list).await,
            serde_json::json!([{ "name": name, "saved": false, "chat_available": false, "remote_chat_granted": null }])
        );
        drop((app, access));
        std::fs::remove_dir_all(root).expect("remove isolated profile");
    }

    #[tokio::test]
    async fn directory_uses_subject_bound_remote_chat_grant() {
        let (app, root, owner, access) = fixture_with_access().await;
        let name = "remote-only.example.dhttp.net";
        let subject = SubjectId::new(b"remote-only-subject".to_vec()).expect("subject");
        access
            .create_contact(NewContact {
                name: name.to_owned(),
                subject_id: subject.clone(),
                class: String::from("Human"),
                description: String::new(),
                requested_access: crate::chat::CHAT_CAPABILITY.requested_access(),
                offers: Default::default(),
                created_at: 1_700_000_000,
                updated_at: 1_700_000_000,
                expired_after: 2_000_000_000,
            })
            .await
            .expect("contact");
        access
            .patch_contact(
                name,
                ContactPatch::LocalUpdate {
                    status: Some(ContactStatus::Active),
                    alias: None,
                },
            )
            .await
            .expect("activate contact");
        let path = "/workspace-api/contact-directory";
        let list = app
            .clone()
            .oneshot(request_to(path, Method::GET, "", Some(owner.clone())))
            .await
            .expect("directory without remote grant");
        assert_eq!(response_json(list).await, serde_json::json!([]));

        let db_path = root.join("owner.example.dhttp.net/db/chat.db");
        let db = Database::connect(format!("sqlite://{}?mode=rw", db_path.display()))
            .await
            .expect("open Chat database");
        db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "INSERT INTO chat_capability_state (contact_name, subject_id, remote_message_granted, updated_at) VALUES (?, ?, 1, 1)",
            [name.into(), subject.as_bytes().to_vec().into()],
        ))
        .await
        .expect("record remote Chat grant");
        let list = app
            .clone()
            .oneshot(request_to(path, Method::GET, "", Some(owner.clone())))
            .await
            .expect("directory with remote grant");
        assert_eq!(
            response_json(list).await,
            serde_json::json!([{ "name": name, "saved": false, "chat_available": true, "remote_chat_granted": true }])
        );

        access
            .delete_contacts(&[name.to_owned()])
            .await
            .expect("remove old contact");
        let replacement = SubjectId::new(b"replacement-subject".to_vec()).expect("replacement");
        access
            .create_contact(NewContact {
                name: name.to_owned(),
                subject_id: replacement,
                class: String::from("Human"),
                description: String::new(),
                requested_access: Default::default(),
                offers: Default::default(),
                created_at: 1_700_000_001,
                updated_at: 1_700_000_001,
                expired_after: 2_000_000_000,
            })
            .await
            .expect("replacement contact");
        access
            .patch_contact(
                name,
                ContactPatch::LocalUpdate {
                    status: Some(ContactStatus::Active),
                    alias: None,
                },
            )
            .await
            .expect("activate replacement");
        let list = app
            .clone()
            .oneshot(request_to(path, Method::GET, "", Some(owner)))
            .await
            .expect("directory after subject replacement");
        assert_eq!(response_json(list).await, serde_json::json!([]));
        drop((app, access, db));
        std::fs::remove_dir_all(root).expect("remove isolated profile");
    }

    #[tokio::test]
    async fn saving_pending_identity_does_not_approve_chat() {
        let (app, root, owner, access) = fixture_with_access().await;
        let name = "pending-saved.example.dhttp.net";
        access
            .create_contact(NewContact {
                name: name.to_owned(),
                subject_id: SubjectId::new(b"pending-saved-subject".to_vec()).expect("subject"),
                class: String::from("Human"),
                description: String::new(),
                requested_access: crate::chat::CHAT_CAPABILITY.requested_access(),
                offers: Default::default(),
                created_at: 1_700_000_000,
                updated_at: 1_700_000_000,
                expired_after: 2_000_000_000,
            })
            .await
            .expect("pending contact");
        let saved = app
            .clone()
            .oneshot(request_to(
                &format!("/workspace-api/contacts/{name}/saved"),
                Method::PUT,
                "",
                Some(owner.clone()),
            ))
            .await
            .expect("save pending identity");
        assert_eq!(saved.status(), StatusCode::NO_CONTENT);
        let list = app
            .clone()
            .oneshot(request_to(
                "/workspace-api/contact-directory",
                Method::GET,
                "",
                Some(owner.clone()),
            ))
            .await
            .expect("saved directory");
        assert_eq!(
            response_json(list).await,
            serde_json::json!([{ "name": name, "saved": true, "chat_available": false, "remote_chat_granted": null }])
        );
        let request = app
            .clone()
            .oneshot(request_to(
                "/workspace-api/capability-requests",
                Method::GET,
                "",
                Some(owner),
            ))
            .await
            .expect("pending request");
        assert_eq!(response_json(request).await[0]["contact_name"], name);
        assert_eq!(
            access
                .find_contact_by_name(name)
                .await
                .expect("contact")
                .status,
            ContactStatus::Pending as i32
        );
        assert_eq!(
            exact_rule_effect(&access, name, "POST", "/std/message").await,
            None
        );
        drop((app, access));
        std::fs::remove_dir_all(root).expect("remove isolated profile");
    }

    #[tokio::test]
    async fn outbound_requests_are_isolated_between_profiles() {
        let (alice, alice_root, alice_owner) = fixture_named("alice.example.dhttp.net", None).await;
        let (bob, bob_root, bob_owner) = fixture_named("bob.example.dhttp.net", None).await;
        let path = "/workspace-api/contact-requests";
        let body = r#"{"target_name":"friend.example","description":"Hello","requested_capabilities":["chat"],"offered_capabilities":["chat"]}"#;
        let sent = alice
            .clone()
            .oneshot(request_to(path, Method::POST, body, Some(alice_owner)))
            .await
            .unwrap();
        assert_eq!(sent.status(), StatusCode::ACCEPTED);
        let alice_id = response_json(sent).await["id"].as_i64().unwrap();
        let bob_list = bob
            .clone()
            .oneshot(request_to(path, Method::GET, "", Some(bob_owner.clone())))
            .await
            .unwrap();
        assert_eq!(response_json(bob_list).await["total"], 0);
        let sent = bob
            .clone()
            .oneshot(request_to(path, Method::POST, body, Some(bob_owner)))
            .await
            .unwrap();
        assert_eq!(sent.status(), StatusCode::ACCEPTED);
        let bob_id = response_json(sent).await["id"].as_i64().unwrap();
        assert_eq!(alice_id, 1);
        assert_eq!(bob_id, 1);
        drop((alice, bob));
        std::fs::remove_dir_all(alice_root).unwrap();
        std::fs::remove_dir_all(bob_root).unwrap();
    }

    #[tokio::test]
    async fn concurrent_sends_queue_once_and_expired_request_can_be_replaced() {
        let (app, root, owner) = fixture_with_transport(None).await;
        let path = "/workspace-api/contact-requests";
        let body = r#"{"target_name":"friend.example","description":"Hello","requested_capabilities":["chat"],"offered_capabilities":["chat"]}"#;
        let (first, second) = tokio::join!(
            app.clone()
                .oneshot(request_to(path, Method::POST, body, Some(owner.clone()))),
            app.clone()
                .oneshot(request_to(path, Method::POST, body, Some(owner.clone()))),
        );
        let statuses = [first.unwrap().status(), second.unwrap().status()];
        assert!(statuses.contains(&StatusCode::ACCEPTED));
        assert!(statuses.contains(&StatusCode::CONFLICT));

        let db_path = root.join("owner.example.dhttp.net/db/workspace.db");
        let db = Database::connect(format!("sqlite://{}?mode=rw", db_path.display()))
            .await
            .unwrap();
        db.execute_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "UPDATE outbound_contact_requests SET expired_after = 0 WHERE status = 'queued'"
                .to_owned(),
        ))
        .await
        .unwrap();
        let listing = app
            .clone()
            .oneshot(request_to(path, Method::GET, "", Some(owner.clone())))
            .await
            .unwrap();
        assert_eq!(response_json(listing).await["items"][0]["status"], "failed");
        let sent_again = app
            .clone()
            .oneshot(request_to(path, Method::POST, body, Some(owner)))
            .await
            .unwrap();
        assert_eq!(sent_again.status(), StatusCode::ACCEPTED);
        drop((app, db));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn context_counts_pending_capability_requests() {
        let (app, root, owner, access) = fixture_with_access().await;
        access
            .create_contact(NewContact {
                name: String::from("requester.example.dhttp.net"),
                subject_id: SubjectId::new(b"requester-subject".to_vec()).expect("subject"),
                class: String::from("Human"),
                description: String::from("Requester"),
                requested_access: crate::chat::CHAT_CAPABILITY.requested_access(),
                offers: Default::default(),
                created_at: 1_700_000_000,
                updated_at: 1_700_000_000,
                expired_after: 2_000_000_000,
            })
            .await
            .expect("incoming capability request");
        let response = app
            .clone()
            .oneshot(request_to(
                "/workspace-api/context",
                Method::GET,
                "",
                Some(owner),
            ))
            .await
            .expect("context response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response_json(response).await["badges"]["incoming_contacts"],
            1
        );
        drop((app, access));
        std::fs::remove_dir_all(root).expect("remove isolated profile");
    }

    #[tokio::test]
    async fn approval_list_combines_pending_and_expired_capability_and_access_requests() {
        let (app, root, owner, access) = fixture_with_access().await;
        let current = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time")
                .as_secs(),
        )
        .expect("current time");
        for (name, expiry) in [
            ("pending-chat.example", current + 3_600),
            ("expired-chat.example", current - 3_600),
            ("approved-chat.example", current - 3_600),
        ] {
            access
                .create_contact(NewContact {
                    name: name.to_owned(),
                    subject_id: SubjectId::new(name.as_bytes().to_vec()).expect("subject"),
                    class: String::from("Human"),
                    description: String::new(),
                    requested_access: crate::chat::CHAT_CAPABILITY.requested_access(),
                    offers: Default::default(),
                    created_at: current - 7_200,
                    updated_at: current - 7_200,
                    expired_after: expiry,
                })
                .await
                .expect("capability request");
        }
        let db = access.database();
        db.execute_unprepared("UPDATE contacts SET status = 4 WHERE name = 'expired-chat.example'")
            .await
            .expect("mark expired capability request");
        db.execute_unprepared(
            "UPDATE contacts SET status = 4 WHERE name = 'approved-chat.example'",
        )
        .await
        .expect("mark previously approved contact expired");
        let approved_request = db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT id FROM contacts WHERE name = 'approved-chat.example'".to_owned(),
            ))
            .await
            .expect("find approved request")
            .expect("approved request row");
        let approved_request_id: i64 = approved_request
            .try_get("", "id")
            .expect("approved request id");
        let workspace_db_path = root.join("owner.example.dhttp.net/db/workspace.db");
        let workspace_db =
            Database::connect(format!("sqlite://{}?mode=rw", workspace_db_path.display()))
                .await
                .expect("open isolated workspace database");
        workspace_db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "INSERT INTO capability_decisions \
             (contact_name, capability_id, decision, descriptor_version, subject_id, request_id, updated_at) \
             VALUES (?, 'chat', 'approved', '1', ?, ?, ?)",
            [
                "approved-chat.example".into(),
                b"approved-chat.example".to_vec().into(),
                approved_request_id.into(),
                (current - 4_000).into(),
            ],
        ))
        .await
        .expect("record prior approval");
        for (request_id, expiry, stage) in [
            ("pending-access", current + 3_600, 0),
            ("expired-access", current - 3_600, 0),
            ("resolved-access", current - 3_600, 1),
        ] {
            db.execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "INSERT INTO access_reviews \
                 (request_id, visitor, visitor_sid, method, api, stage, reason, expired_after, updated_at, created_at) \
                 VALUES (?, 'visitor.example', X'01', 'GET', '/files', ?, 'review', ?, ?, ?)",
                [request_id.into(), stage.into(), expiry.into(), (current - 7_200).into(), (current - 7_200).into()],
            ))
            .await
            .expect("access review");
        }

        for status in ["pending", "expired"] {
            let path = format!("/workspace-api/approvals?status={status}&page=1&page_size=20");
            let response = app
                .clone()
                .oneshot(request_to(&path, Method::GET, "", Some(owner.clone())))
                .await
                .expect("approval list");
            assert_eq!(response.status(), StatusCode::OK);
            let page = response_json(response).await;
            assert_eq!(page["total"], 2);
            let items = page["items"].as_array().expect("items");
            assert!(items.iter().any(|item| item["kind"] == "capability"));
            assert!(items.iter().any(|item| item["kind"] == "access"));
            assert!(
                !items
                    .iter()
                    .any(|item| item["contact_name"] == "approved-chat.example")
            );
            assert!(
                items
                    .iter()
                    .filter(|item| item["kind"] == "capability")
                    .all(|item| item["requested_at"].as_i64() == Some(current - 7_200))
            );
            let first_page = app
                .clone()
                .oneshot(request_to(
                    &format!("/workspace-api/approvals?status={status}&page=1&page_size=1"),
                    Method::GET,
                    "",
                    Some(owner.clone()),
                ))
                .await
                .expect("paginated approvals");
            assert_eq!(
                response_json(first_page).await["items"]
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
        }
        let denied = app
            .clone()
            .oneshot(request_to(
                "/workspace-api/approvals",
                Method::GET,
                "",
                None,
            ))
            .await
            .expect("owner guard");
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
        let invalid = app
            .clone()
            .oneshot(request_to(
                "/workspace-api/approvals?status=pending&page=0",
                Method::GET,
                "",
                Some(owner),
            ))
            .await
            .expect("invalid pagination");
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
        drop((app, access, workspace_db));
        std::fs::remove_dir_all(root).expect("remove isolated profile");
    }

    #[tokio::test]
    async fn expired_approvals_can_be_deleted_only_by_the_owner() {
        let (app, root, owner, access) = fixture_with_access().await;
        let current = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time")
                .as_secs(),
        )
        .expect("current time");
        let name = "expired-delete.example";
        access
            .create_contact(NewContact {
                name: name.to_owned(),
                subject_id: SubjectId::new(name.as_bytes().to_vec()).expect("subject"),
                class: String::from("Human"),
                description: String::new(),
                requested_access: crate::chat::CHAT_CAPABILITY.requested_access(),
                offers: Default::default(),
                created_at: current - 7_200,
                updated_at: current - 7_200,
                expired_after: current - 3_600,
            })
            .await
            .expect("expired capability request");
        access
            .database()
            .execute_unprepared(
                "UPDATE contacts SET status = 4 WHERE name = 'expired-delete.example'",
            )
            .await
            .expect("mark capability request expired");
        let row = access
            .database()
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT id FROM contacts WHERE name = ?",
                [name.into()],
            ))
            .await
            .expect("find expired capability request")
            .expect("capability request row");
        let capability_id: i64 = row.try_get("", "id").expect("request id");
        access
            .database()
            .execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "INSERT INTO access_reviews \
                 (request_id, visitor, visitor_sid, method, api, stage, reason, expired_after, updated_at, created_at) \
                 VALUES ('expired-delete', 'visitor.example', X'01', 'GET', '/files', 0, 'review', ?, ?, ?)",
                [(current - 3_600).into(), (current - 7_200).into(), (current - 7_200).into()],
            ))
            .await
            .expect("insert expired access approval");
        access
            .database()
            .execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "INSERT INTO access_reviews \
                 (request_id, visitor, visitor_sid, method, api, stage, reason, expired_after, updated_at, created_at) \
                 VALUES ('pending-keep', 'visitor.example', X'01', 'POST', '/files', 0, 'review', ?, ?, ?)",
                [(current + 3_600).into(), current.into(), current.into()],
            ))
            .await
            .expect("insert pending access approval");
        let expired_row = access
            .database()
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT id FROM access_reviews WHERE request_id = 'expired-delete'",
                [],
            ))
            .await
            .expect("find expired access approval")
            .expect("expired access approval row");
        let access_id: i64 = expired_row.try_get("", "id").expect("approval id");

        let unauthorized = app
            .clone()
            .oneshot(request_to(
                &format!("/workspace-api/approvals/access/{access_id}"),
                Method::DELETE,
                "",
                None,
            ))
            .await
            .expect("owner guard");
        assert_eq!(unauthorized.status(), StatusCode::FORBIDDEN);
        let delete_access = app
            .clone()
            .oneshot(request_to(
                &format!("/workspace-api/approvals/access/{access_id}"),
                Method::DELETE,
                "",
                Some(owner.clone()),
            ))
            .await
            .expect("delete expired access approval");
        assert_eq!(delete_access.status(), StatusCode::NO_CONTENT);
        let delete_capability = app
            .clone()
            .oneshot(request_to(
                &format!("/workspace-api/approvals/capability/{capability_id}"),
                Method::DELETE,
                "",
                Some(owner.clone()),
            ))
            .await
            .expect("delete expired capability approval");
        assert_eq!(delete_capability.status(), StatusCode::NO_CONTENT);
        let delete_pending = app
            .clone()
            .oneshot(request_to(
                "/workspace-api/approvals/access/2",
                Method::DELETE,
                "",
                Some(owner.clone()),
            ))
            .await
            .expect("reject non-expired approval deletion");
        assert_eq!(delete_pending.status(), StatusCode::NOT_FOUND);
        let repeat_delete = app
            .clone()
            .oneshot(request_to(
                &format!("/workspace-api/approvals/access/{access_id}"),
                Method::DELETE,
                "",
                Some(owner),
            ))
            .await
            .expect("reject repeated approval deletion");
        assert_eq!(repeat_delete.status(), StatusCode::NOT_FOUND);
        assert!(access.find_contact_by_name(name).await.is_err());

        drop((app, access));
        std::fs::remove_dir_all(root).expect("remove isolated profile");
    }

    #[tokio::test]
    async fn denied_chat_request_can_be_submitted_again() {
        let (app, root, owner, access) = fixture_with_access().await;
        let name = "requester.example.dhttp.net";
        let subject_id = SubjectId::new(b"requester-subject".to_vec()).expect("subject");
        access
            .create_contact(NewContact {
                name: name.to_owned(),
                subject_id: subject_id.clone(),
                class: String::from("Human"),
                description: String::from("Requester"),
                requested_access: crate::chat::CHAT_CAPABILITY.requested_access(),
                offers: Default::default(),
                created_at: 1_700_000_000,
                updated_at: 1_700_000_000,
                expired_after: 2_000_000_000,
            })
            .await
            .expect("incoming capability request");

        let list = app
            .clone()
            .oneshot(request_to(
                "/workspace-api/capability-requests",
                Method::GET,
                "",
                Some(owner.clone()),
            ))
            .await
            .expect("list capability requests");
        let listed = response_json(list).await;
        assert_eq!(listed[0]["contact_name"], name);
        let request_id = listed[0]["request_id"].as_i64().expect("request id");

        let deny_path = format!(
            "/workspace-api/contacts/{name}/capabilities/chat/deny?request_id={request_id}&capability_version=1"
        );
        for _ in 0..2 {
            let response = app
                .clone()
                .oneshot(request_to(
                    &deny_path,
                    Method::POST,
                    "",
                    Some(owner.clone()),
                ))
                .await
                .expect("deny capability request");
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
        }
        assert!(access.find_contact_by_name(name).await.is_err());
        let context = app
            .clone()
            .oneshot(request_to(
                "/workspace-api/context",
                Method::GET,
                "",
                Some(owner.clone()),
            ))
            .await
            .expect("context after denial");
        assert_eq!(
            response_json(context).await["badges"]["incoming_contacts"],
            0
        );

        let now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time")
                .as_secs(),
        )
        .expect("current time");
        access
            .create_contact(NewContact {
                name: name.to_owned(),
                subject_id,
                class: String::from("Human"),
                description: String::from("Requested again"),
                requested_access: crate::chat::CHAT_CAPABILITY.requested_access(),
                offers: Default::default(),
                created_at: now,
                updated_at: now,
                expired_after: now + 3_600,
            })
            .await
            .expect("resubmit capability request");
        let list = app
            .clone()
            .oneshot(request_to(
                "/workspace-api/capability-requests",
                Method::GET,
                "",
                Some(owner.clone()),
            ))
            .await
            .expect("list resubmitted request");
        let listed_again = response_json(list).await;
        assert_eq!(listed_again[0]["contact_name"], name);
        assert_ne!(listed_again[0]["request_id"], request_id);
        let stale = app
            .clone()
            .oneshot(request_to(
                &deny_path,
                Method::POST,
                "",
                Some(owner.clone()),
            ))
            .await
            .expect("stale decision response");
        assert_eq!(stale.status(), StatusCode::CONFLICT);

        let grant_path = format!("/workspace-api/contacts/{name}/capabilities/chat/grant");
        let stale_grant = app
            .clone()
            .oneshot(request_to(
                &format!("{grant_path}?request_id={request_id}&capability_version=1"),
                Method::POST,
                "",
                Some(owner.clone()),
            ))
            .await
            .expect("stale grant response");
        assert_eq!(stale_grant.status(), StatusCode::CONFLICT);

        let approved = app
            .clone()
            .oneshot(request_to(
                &format!(
                    "{grant_path}?request_id={}&capability_version=1",
                    listed_again[0]["request_id"]
                        .as_i64()
                        .expect("new request id")
                ),
                Method::POST,
                "",
                Some(owner),
            ))
            .await
            .expect("approve resubmitted request");
        assert_eq!(approved.status(), StatusCode::NO_CONTENT);
        let db_path = root.join("owner.example.dhttp.net/db/workspace.db");
        let db = Database::connect(format!("sqlite://{}?mode=rw", db_path.display()))
            .await
            .expect("open workspace database");
        let events = db
            .query_all_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT decision FROM capability_decision_events ORDER BY id".to_owned(),
            ))
            .await
            .expect("read capability decision events");
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].try_get::<String>("", "decision").expect("denial"),
            "denied"
        );
        assert_eq!(
            events[1]
                .try_get::<String>("", "decision")
                .expect("approval"),
            "approved"
        );

        drop((app, access, db));
        std::fs::remove_dir_all(root).expect("remove isolated profile");
    }

    #[tokio::test]
    async fn profile_settings_are_owner_only_and_persist_locally() {
        let (app, root, owner) = fixture().await;

        let missing = app
            .clone()
            .oneshot(request(Method::GET, "", None))
            .await
            .expect("missing visitor response");
        assert_eq!(missing.status(), StatusCode::FORBIDDEN);
        let context_without_owner = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/workspace-api/context")
                    .body(Body::empty())
                    .expect("valid context request"),
            )
            .await
            .expect("context response");
        assert_eq!(context_without_owner.status(), StatusCode::FORBIDDEN);
        let old_certificate = app
            .clone()
            .oneshot(request(
                Method::PATCH,
                r#"{"display_name":"Impostor"}"#,
                Some(Visitor::new(
                    "owner.example",
                    SubjectId::new(b"old-key".to_vec()).expect("old subject"),
                )),
            ))
            .await
            .expect("wrong certificate response");
        assert_eq!(old_certificate.status(), StatusCode::FORBIDDEN);

        let invalid = app
            .clone()
            .oneshot(request(
                Method::PATCH,
                &serde_json::json!({"display_name": "x".repeat(81)}).to_string(),
                Some(owner.clone()),
            ))
            .await
            .expect("invalid profile response");
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);

        let response = app
            .clone()
            .oneshot(request(
                Method::PATCH,
                r#"{"display_name":"  Alice  "}"#,
                Some(owner.clone()),
            ))
            .await
            .expect("owner update response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        assert_eq!(body["identity_name"], "owner.example");
        assert_eq!(body["display_name"], "Alice");
        let profile_updated = body["updated_at"].as_i64().expect("profile timestamp");

        let response = app
            .clone()
            .oneshot(request(Method::GET, "", Some(owner.clone())))
            .await
            .expect("owner profile response");
        let body = response_json(response).await;
        assert_eq!(body["display_name"], "Alice");

        let avatar = Bytes::from_static(include_bytes!("../../assets/pishoo/pishoo-icon.jpg"));
        let avatar_request = |visitor: Option<Visitor>| {
            let mut request = request_to(
                "/workspace-api/settings/profile/avatar",
                Method::PUT,
                "",
                visitor,
            );
            request.headers_mut().insert(
                header::CONTENT_TYPE,
                http::HeaderValue::from_static("image/jpeg"),
            );
            *request.body_mut() = Body::from(avatar.clone());
            request
        };
        let denied = app
            .clone()
            .oneshot(avatar_request(None))
            .await
            .expect("avatar owner check");
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
        let mut mismatched = avatar_request(Some(owner.clone()));
        mismatched.headers_mut().insert(
            header::CONTENT_TYPE,
            http::HeaderValue::from_static("image/png"),
        );
        let mismatched = app
            .clone()
            .oneshot(mismatched)
            .await
            .expect("avatar content type check");
        assert_eq!(mismatched.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let uploaded = app
            .clone()
            .oneshot(avatar_request(Some(owner.clone())))
            .await
            .expect("avatar upload");
        assert_eq!(uploaded.status(), StatusCode::OK);
        let uploaded = response_json(uploaded).await;
        assert_eq!(
            uploaded["avatar_url"],
            "/workspace-api/settings/profile/avatar"
        );
        let avatar_updated = uploaded["updated_at"].as_i64().expect("avatar timestamp");
        assert!(avatar_updated > profile_updated);

        let public = app
            .clone()
            .oneshot(request_to("/std/profile", Method::GET, "", None))
            .await
            .expect("public profile");
        assert_eq!(public.status(), StatusCode::OK);
        assert_eq!(
            public.headers()[header::CACHE_CONTROL],
            "public, max-age=300, must-revalidate"
        );
        let public = response_json(public).await;
        assert_eq!(public["display_name"], "Alice");
        assert_eq!(public["avatar_url"], "/std/profile/avatar");

        let public_avatar = app
            .clone()
            .oneshot(request_to("/std/profile/avatar", Method::GET, "", None))
            .await
            .expect("public avatar");
        assert_eq!(public_avatar.status(), StatusCode::OK);
        assert_eq!(public_avatar.headers()[header::CONTENT_TYPE], "image/jpeg");
        assert_eq!(
            public_avatar.headers()[header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );
        let etag = public_avatar.headers()[header::ETAG].clone();
        let mut conditional = request_to("/std/profile/avatar", Method::GET, "", None);
        conditional
            .headers_mut()
            .insert(header::IF_NONE_MATCH, etag);
        let not_modified = app
            .clone()
            .oneshot(conditional)
            .await
            .expect("conditional avatar");
        assert_eq!(not_modified.status(), StatusCode::NOT_MODIFIED);

        let deleted = app
            .clone()
            .oneshot(request_to(
                "/workspace-api/settings/profile/avatar",
                Method::DELETE,
                "",
                Some(owner),
            ))
            .await
            .expect("avatar deletion");
        let deleted = response_json(deleted).await;
        assert_eq!(deleted["avatar_url"], serde_json::Value::Null);
        assert!(deleted["updated_at"].as_i64().expect("delete timestamp") > avatar_updated);

        drop(app);
        std::fs::remove_dir_all(root).expect("remove isolated test profile");
    }

    #[tokio::test]
    async fn capability_catalog_is_owner_only_and_contains_builtin_descriptors() {
        let (app, root, owner) = fixture().await;
        let denied = app
            .clone()
            .oneshot(request_to(
                "/workspace-api/capabilities",
                Method::GET,
                "",
                None,
            ))
            .await
            .expect("capability catalog denial");
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
        let response = app
            .clone()
            .oneshot(request_to(
                "/workspace-api/capabilities",
                Method::GET,
                "",
                Some(owner),
            ))
            .await
            .expect("capability catalog");
        let descriptors = response_json(response).await;
        assert_eq!(descriptors[0]["id"], "public_profile");
        assert_eq!(descriptors[1]["id"], "chat");
        assert_eq!(descriptors[1]["approval_mode"], "capability");
        assert_eq!(descriptors[1]["selectable"], true);
        drop(app);
        std::fs::remove_dir_all(root).expect("remove isolated test profile");
    }

    #[tokio::test]
    async fn remote_profiles_are_proxied_without_a_server_cache() {
        let remote = Arc::new(FakeOutbound::default());
        let (app, root, owner) = fixture_with_transport(Some(remote.clone())).await;
        let path = "/workspace-api/profiles/friend.example";
        let denied = app
            .clone()
            .oneshot(request_to(path, Method::GET, "", None))
            .await
            .expect("profile owner check");
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);

        for _ in 0..2 {
            let response = app
                .clone()
                .oneshot(request_to(path, Method::GET, "", Some(owner.clone())))
                .await
                .expect("remote profile");
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.headers()[header::CACHE_CONTROL],
                "private, max-age=300, must-revalidate"
            );
            let body = response_json(response).await;
            assert_eq!(body["display_name"], "Remote friend");
            assert_eq!(
                body["avatar_url"],
                "/workspace-api/profiles/friend.example.dhttp.net/avatar?v=1700000000"
            );
        }
        let profile_requests = remote
            .requests
            .lock()
            .expect("log")
            .iter()
            .filter(|request| request.2 == "/std/profile")
            .count();
        assert_eq!(profile_requests, 2, "pishoo must not cache remote profiles");

        let avatar = app
            .clone()
            .oneshot(request_to(
                "/workspace-api/profiles/friend.example/avatar",
                Method::GET,
                "",
                Some(owner),
            ))
            .await
            .expect("remote avatar");
        assert_eq!(avatar.status(), StatusCode::OK);
        assert_eq!(avatar.headers()[header::CONTENT_TYPE], "image/jpeg");
        assert_eq!(
            avatar.headers()[header::CACHE_CONTROL],
            "private, max-age=86400, immutable"
        );
        drop(app);
        std::fs::remove_dir_all(root).expect("remove isolated test profile");
    }

    #[tokio::test]
    async fn unavailable_remote_profiles_and_avatars_have_a_short_deadline() {
        let remote = Arc::new(FakeOutbound::default());
        remote.hang_profiles.store(true, Ordering::SeqCst);
        let (app, root, owner) = fixture_with_transport(Some(remote)).await;
        tokio::time::pause();

        for path in [
            "/workspace-api/profiles/offline.example",
            "/workspace-api/profiles/offline.example/avatar",
        ] {
            let started = tokio::time::Instant::now();
            let response = app
                .clone()
                .oneshot(request_to(path, Method::GET, "", Some(owner.clone())))
                .await
                .expect("bounded remote profile request");
            assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
            let elapsed = started.elapsed();
            let timeout = super::profile::REMOTE_PROFILE_TIMEOUT;
            assert!(elapsed >= timeout);
            assert!(elapsed <= timeout + std::time::Duration::from_millis(5));
        }
        tokio::time::resume();

        let local = app
            .clone()
            .oneshot(request_to(
                "/workspace-api/context",
                Method::GET,
                "",
                Some(owner),
            ))
            .await
            .expect("local context remains available");
        assert_eq!(local.status(), StatusCode::OK);
        drop(app);
        std::fs::remove_dir_all(root).expect("remove isolated test profile");
    }

    #[tokio::test]
    async fn standard_messages_are_contact_bound_and_idempotent() {
        let (app, root, _owner, access) = fixture_with_access().await;
        let contact_name = "alice.example.dhttp.net";
        let contact_subject = SubjectId::new(b"alice-subject".to_vec()).expect("contact subject");
        access
            .create_contact(NewContact {
                name: contact_name.to_owned(),
                subject_id: contact_subject.clone(),
                class: String::from("Human"),
                description: String::from("Alice"),
                requested_access: Default::default(),
                offers: Default::default(),
                created_at: 1_700_000_000,
                updated_at: 1_700_000_000,
                expired_after: 2_000_000_000,
            })
            .await
            .expect("create message contact");
        access
            .patch_contact(
                contact_name,
                ContactPatch::LocalUpdate {
                    status: Some(ContactStatus::Active),
                    alias: None,
                },
            )
            .await
            .expect("activate message contact");
        let visitor = Visitor::new(contact_name, contact_subject);
        let body = r#"{"client_message_id":"msg-1","text":"hello"}"#;
        let created = app
            .clone()
            .oneshot(request_to(
                "/std/message",
                Method::POST,
                body,
                Some(visitor.clone()),
            ))
            .await
            .expect("post message");
        assert_eq!(created.status(), StatusCode::OK);
        let created = response_json(created).await;
        assert_eq!(created["sender"], contact_name);
        assert_eq!(created["recipient"], "owner.example.dhttp.net");
        assert_eq!(created["text"], "hello");
        let id = created["id"].as_str().expect("message id").to_owned();

        let repeated = app
            .clone()
            .oneshot(request_to(
                "/std/message",
                Method::POST,
                body,
                Some(visitor.clone()),
            ))
            .await
            .expect("repeat message");
        assert_eq!(repeated.status(), StatusCode::OK);
        assert_eq!(response_json(repeated).await["id"], id);

        let conflict = app
            .clone()
            .oneshot(request_to(
                "/std/message",
                Method::POST,
                r#"{"client_message_id":"msg-1","text":"changed"}"#,
                Some(visitor.clone()),
            ))
            .await
            .expect("message id conflict");
        assert_eq!(conflict.status(), StatusCode::CONFLICT);

        let listed = app
            .clone()
            .oneshot(request_to(
                "/std/message?after=0&limit=1",
                Method::GET,
                "",
                Some(visitor.clone()),
            ))
            .await
            .expect("list messages");
        assert_eq!(listed.status(), StatusCode::METHOD_NOT_ALLOWED);

        let unknown = app
            .clone()
            .oneshot(request_to(
                "/std/message",
                Method::POST,
                r#"{"client_message_id":"msg-2","text":"hello","sender":"mallory"}"#,
                Some(visitor.clone()),
            ))
            .await
            .expect("unknown message field");
        assert_eq!(unknown.status(), StatusCode::BAD_REQUEST);

        let mismatch = app
            .clone()
            .oneshot(request_to(
                "/std/message",
                Method::GET,
                "",
                Some(Visitor::new(
                    contact_name,
                    SubjectId::new(b"wrong-subject".to_vec()).expect("wrong subject"),
                )),
            ))
            .await
            .expect("subject mismatch");
        assert_eq!(mismatch.status(), StatusCode::METHOD_NOT_ALLOWED);

        let anonymous = app
            .oneshot(request_to("/std/message", Method::GET, "", None))
            .await
            .expect("anonymous message request");
        assert_eq!(anonymous.status(), StatusCode::METHOD_NOT_ALLOWED);
        drop(access);
        std::fs::remove_dir_all(root).expect("remove message test profile");
    }
}
