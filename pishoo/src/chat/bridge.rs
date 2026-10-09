use std::sync::Arc;

use axum::{
    Extension, Json,
    extract::{Path, Query, State, rejection::JsonRejection},
};
use http::StatusCode;
use serde::Deserialize;

use super::{
    Chat,
    service::{self, ConversationPage, ServiceError},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MessageQuery {
    after: Option<String>,
    limit: Option<u16>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SendMessage {
    text: String,
}

pub(crate) async fn get_messages(
    State(state): State<Arc<Chat>>,
    visitor: Option<Extension<access_control::Visitor>>,
    Path(name): Path<String>,
    Query(query): Query<MessageQuery>,
) -> Result<Json<ConversationPage>, ServiceError> {
    service::require_owner(&state, visitor.as_ref().map(|extension| &extension.0))?;
    let target = service::resolve_target(&state, &name).await?;
    Ok(Json(
        service::list_messages(&state, &target, query.after, query.limit).await?,
    ))
}

pub(crate) async fn post_message(
    State(state): State<Arc<Chat>>,
    visitor: Option<Extension<access_control::Visitor>>,
    Path(name): Path<String>,
    body: Result<Json<SendMessage>, JsonRejection>,
) -> Result<(StatusCode, Json<service::LocalMessage>), ServiceError> {
    service::require_owner(&state, visitor.as_ref().map(|extension| &extension.0))?;
    let body = body
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid Chat message submission"))?
        .0;
    let target = service::resolve_target(&state, &name).await?;
    let message = service::send_message(&state, &target, body.text).await?;
    Ok((StatusCode::ACCEPTED, Json(message)))
}

pub(crate) async fn requeue_message(
    State(state): State<Arc<Chat>>,
    visitor: Option<Extension<access_control::Visitor>>,
    Path((name, id)): Path<(String, String)>,
) -> Result<Json<service::LocalMessage>, ServiceError> {
    service::require_owner(&state, visitor.as_ref().map(|extension| &extension.0))?;
    let target = service::resolve_target(&state, &name).await?;
    Ok(Json(service::requeue_message(&state, &target, &id).await?))
}

pub(crate) async fn get_capability(
    State(state): State<Arc<Chat>>,
    visitor: Option<Extension<access_control::Visitor>>,
    Path(name): Path<String>,
) -> Result<Json<service::CapabilityState>, ServiceError> {
    service::require_owner(&state, visitor.as_ref().map(|extension| &extension.0))?;
    Ok(Json(service::capability_state(&state, &name).await?))
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
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
    use http::{Method, Request, StatusCode};
    use tokio::sync::Notify;
    use tokio_util::sync::CancellationToken;
    use tower::ServiceExt;

    use crate::chat::{
        Chat,
        message::MessageEnvelope,
        outbound::{OutboundTransport, RemoteResponse},
        router,
        store::ChatStore,
    };

    static NEXT_FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

    #[derive(Clone)]
    struct FakeOutbound {
        requests: Arc<Mutex<Vec<(Method, String, Bytes)>>>,
        response: Arc<Mutex<RemoteResponse>>,
        identity_changed: Arc<AtomicBool>,
    }

    impl OutboundTransport for FakeOutbound {
        fn request<'a>(
            &'a self,
            _target: &'a str,
            _expected_subject_id: &'a SubjectId,
            method: Method,
            path: &'a str,
            body: Bytes,
        ) -> BoxFuture<'a, Result<RemoteResponse, String>> {
            let requests = self.requests.clone();
            let response = self.response.clone();
            let identity_changed = self.identity_changed.clone();
            Box::pin(async move {
                if identity_changed.load(Ordering::SeqCst) {
                    return Err(String::from(crate::chat::outbound::REMOTE_IDENTITY_CHANGED));
                }
                requests
                    .lock()
                    .expect("request log")
                    .push((method, path.to_owned(), body));
                let response = response.lock().expect("response");
                Ok(RemoteResponse {
                    status: response.status,
                    body: response.body.clone(),
                })
            })
        }
    }

    struct Fixture {
        root: std::path::PathBuf,
        app: axum::Router,
        state: Arc<Chat>,
        owner: Visitor,
        access: Arc<AccessService>,
        remote: Arc<Mutex<RemoteResponse>>,
        requests: Arc<Mutex<Vec<(Method, String, Bytes)>>>,
        identity_changed: Arc<AtomicBool>,
    }

    async fn fixture() -> Fixture {
        let root = std::env::temp_dir().join(format!(
            "pishoo-chat-bridge-{}-{}",
            std::process::id(),
            NEXT_FIXTURE_ID.fetch_add(1, Ordering::Relaxed),
        ));
        let profile = IdentityProfile::try_from(root.join("owner.example")).expect("profile");
        let subject = SubjectId::new(b"owner-subject".to_vec()).expect("owner subject");
        let access = Arc::new(
            AccessService::load_from_db("sqlite::memory:", "owner.example", &subject)
                .await
                .expect("access db"),
        );
        access
            .create_contact(NewContact {
                name: String::from("friend.example.dhttp.net"),
                subject_id: SubjectId::new(b"friend-subject".to_vec()).expect("friend subject"),
                class: String::from("Human"),
                description: String::from("Friend"),
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
                "friend.example.dhttp.net",
                ContactPatch::LocalUpdate {
                    status: Some(ContactStatus::Active),
                    alias: None,
                },
            )
            .await
            .expect("activate contact");
        let store = ChatStore::open(&profile).await.expect("Chat store");
        let state = Arc::new(Chat::new(
            String::from("owner.example"),
            subject.clone(),
            store,
            access.clone(),
        ));
        state
            .update_remote_chat_grant(
                "friend.example.dhttp.net",
                &SubjectId::new(b"friend-subject".to_vec()).expect("friend subject"),
                &crate::chat::CHAT_CAPABILITY.offers(),
            )
            .await
            .expect("remote Chat grant");
        state.start_worker();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let identity_changed = Arc::new(AtomicBool::new(false));
        let remote = Arc::new(Mutex::new(RemoteResponse {
            status: StatusCode::OK,
            body: Bytes::from(
                serde_json::to_vec(&MessageEnvelope {
                    id: String::from("remote-send-1"),
                    client_message_id: String::from("client-remote"),
                    sender: String::from("owner.example"),
                    recipient: String::from("friend.example.dhttp.net"),
                    text: String::from("hello"),
                    created_at: 1_700_000_001,
                })
                .expect("message JSON"),
            ),
        }));
        state
            .configure_outbound(Arc::new(FakeOutbound {
                requests: requests.clone(),
                response: remote.clone(),
                identity_changed: identity_changed.clone(),
            }))
            .await;
        let app = router(state.clone());
        Fixture {
            root,
            app,
            state,
            owner: Visitor::new("owner.example", subject),
            access,
            remote,
            requests,
            identity_changed,
        }
    }

    fn request(path: &str, method: Method, body: &str, visitor: Option<Visitor>) -> Request<Body> {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_owned()))
            .expect("request");
        if let Some(visitor) = visitor {
            request.extensions_mut().insert(visitor);
        }
        request
    }

    async fn json(response: Response) -> serde_json::Value {
        serde_json::from_slice(
            &to_bytes(response.into_body(), 64 * 1024)
                .await
                .expect("response body"),
        )
        .expect("JSON response")
    }

    async fn wait_for_state(fixture: &Fixture, expected: &str) -> serde_json::Value {
        for _ in 0..100 {
            let response = fixture
                .app
                .clone()
                .oneshot(request(
                    "/std/chat-api/conversations/friend.example.dhttp.net/messages",
                    Method::GET,
                    "",
                    Some(fixture.owner.clone()),
                ))
                .await
                .expect("list response");
            let body = json(response).await;
            if body["items"][0]["state"] == expected {
                return body;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("message did not reach state {expected}");
    }

    #[tokio::test]
    async fn bridge_sends_owner_message_and_exposes_sent_state() {
        let fixture = fixture().await;
        let anonymous = fixture
            .app
            .clone()
            .oneshot(request(
                "/std/chat-api/conversations/friend.example.dhttp.net/messages",
                Method::POST,
                r#"{"text":"hello"}"#,
                None,
            ))
            .await
            .expect("anonymous response");
        assert_eq!(anonymous.status(), StatusCode::FORBIDDEN);

        let response = fixture
            .app
            .clone()
            .oneshot(request(
                "/std/chat-api/conversations/friend.example.dhttp.net/messages",
                Method::POST,
                r#"{"text":"hello"}"#,
                Some(fixture.owner.clone()),
            ))
            .await
            .expect("send response");
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let sent = json(response).await;
        assert_eq!(sent["state"], "queued");
        wait_for_state(&fixture, "sent").await;
        let requests = fixture.requests.lock().expect("request log");
        assert!(
            requests
                .iter()
                .any(|request| request.0 == Method::POST && request.1 == "/std/message")
        );
        drop(requests);

        let listed = fixture
            .app
            .clone()
            .oneshot(request(
                "/std/chat-api/conversations/friend.example.dhttp.net/messages",
                Method::GET,
                "",
                Some(fixture.owner.clone()),
            ))
            .await
            .expect("list response");
        assert_eq!(listed.status(), StatusCode::OK);
        let listed = json(listed).await;
        assert_eq!(listed["items"][0]["state"], "sent");
        drop(fixture.access);
        std::fs::remove_dir_all(fixture.root).expect("remove fixture");
    }

    #[tokio::test]
    async fn capability_state_distinguishes_active_and_blocked_contacts() {
        let fixture = fixture().await;
        let path = "/std/chat-api/conversations/friend.example.dhttp.net/capability";
        let initial = fixture
            .app
            .clone()
            .oneshot(request(path, Method::GET, "", Some(fixture.owner.clone())))
            .await
            .expect("waiting response");
        assert_eq!(initial.status(), StatusCode::OK);
        let initial = json(initial).await;
        assert_eq!(initial["status"], "available");
        assert_eq!(initial["can_send"], true);
        assert_eq!(initial["can_receive"], false);
        assert_eq!(initial["remote_grant"], true);

        let grantee = Grantee::One(String::from("friend.example.dhttp.net"));
        fixture
            .access
            .set_policy(
                AccessMethod::Specified(Method::POST),
                "/std/message",
                Effect::Allow,
                grantee,
            )
            .await
            .expect("Chat rule");
        let available = fixture
            .app
            .clone()
            .oneshot(request(path, Method::GET, "", Some(fixture.owner.clone())))
            .await
            .expect("available response");
        let available = json(available).await;
        assert_eq!(available["status"], "available");
        assert_eq!(available["can_receive"], true);

        fixture
            .access
            .remove_policy(
                AccessMethod::Specified(Method::POST),
                "/std/message",
                Grantee::One(String::from("friend.example.dhttp.net")),
            )
            .await
            .expect("revoke Chat rule");
        let revoked = fixture
            .app
            .clone()
            .oneshot(request(path, Method::GET, "", Some(fixture.owner.clone())))
            .await
            .expect("revoked capability response");
        assert_eq!(json(revoked).await["can_receive"], false);
        fixture
            .access
            .set_policy(
                AccessMethod::Specified(Method::POST),
                "/std/message",
                Effect::Allow,
                Grantee::One(String::from("friend.example.dhttp.net")),
            )
            .await
            .expect("restore Chat rule");

        fixture
            .access
            .patch_contact(
                "friend.example.dhttp.net",
                ContactPatch::LocalUpdate {
                    status: Some(ContactStatus::Blocked),
                    alias: None,
                },
            )
            .await
            .expect("block contact");
        let blocked = fixture
            .app
            .clone()
            .oneshot(request(path, Method::GET, "", Some(fixture.owner)))
            .await
            .expect("blocked response");
        let blocked = json(blocked).await;
        assert_eq!(blocked["status"], "blocked");
        assert_eq!(blocked["can_send"], false);
        assert_eq!(blocked["can_receive"], false);
        drop(fixture.access);
        std::fs::remove_dir_all(fixture.root).expect("remove fixture");
    }

    #[tokio::test]
    async fn bridge_keeps_failed_outbound_messages_for_requeue() {
        let fixture = fixture().await;
        fixture.remote.lock().expect("remote").status = StatusCode::FORBIDDEN;
        let response = fixture
            .app
            .clone()
            .oneshot(request(
                "/std/chat-api/conversations/friend.example.dhttp.net/messages",
                Method::POST,
                r#"{"text":"will fail"}"#,
                Some(fixture.owner.clone()),
            ))
            .await
            .expect("failed send response");
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let listed = wait_for_state(&fixture, "blocked").await;
        assert_eq!(listed["items"][0]["state"], "blocked");
        assert_eq!(
            listed["items"][0]["error_message"],
            "remote Chat delivery is not granted"
        );
        let id = listed["items"][0]["id"].as_str().expect("local id");
        fixture.remote.lock().expect("remote").status = StatusCode::OK;
        let retried = fixture
            .app
            .clone()
            .oneshot(request(
                &format!(
                    "/std/chat-api/conversations/friend.example.dhttp.net/messages/{id}/requeue"
                ),
                Method::POST,
                "",
                Some(fixture.owner.clone()),
            ))
            .await
            .expect("requeue response");
        assert_eq!(retried.status(), StatusCode::OK);
        assert_eq!(json(retried).await["state"], "queued");
        assert_eq!(
            wait_for_state(&fixture, "sent").await["items"][0]["state"],
            "sent"
        );
        drop(fixture.access);
        std::fs::remove_dir_all(fixture.root).expect("remove fixture");
    }

    #[tokio::test]
    async fn remote_chat_grant_requeues_blocked_messages() {
        let fixture = fixture().await;
        fixture.remote.lock().expect("remote").status = StatusCode::FORBIDDEN;
        fixture
            .state
            .update_remote_chat_grant(
                "friend.example.dhttp.net",
                &SubjectId::new(b"friend-subject".to_vec()).expect("friend subject"),
                &Default::default(),
            )
            .await
            .expect("remote Chat grant cache update");
        let response = fixture
            .app
            .clone()
            .oneshot(request(
                "/std/chat-api/conversations/friend.example.dhttp.net/messages",
                Method::POST,
                r#"{"text":"wait for grant"}"#,
                Some(fixture.owner.clone()),
            ))
            .await
            .expect("send response");
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            wait_for_state(&fixture, "blocked").await["items"][0]["state"],
            "blocked"
        );

        fixture.remote.lock().expect("remote").status = StatusCode::OK;
        fixture
            .state
            .update_remote_chat_grant(
                "friend.example.dhttp.net",
                &SubjectId::new(b"friend-subject".to_vec()).expect("friend subject"),
                &crate::chat::CHAT_CAPABILITY.offers(),
            )
            .await
            .expect("remote Chat grant");
        assert_eq!(
            wait_for_state(&fixture, "sent").await["items"][0]["state"],
            "sent"
        );
        drop(fixture.access);
        std::fs::remove_dir_all(fixture.root).expect("remove fixture");
    }

    #[tokio::test]
    async fn queued_message_and_cached_grant_do_not_follow_a_replaced_subject() {
        let fixture = fixture().await;
        fixture.state.shutdown().await;
        let name = "friend.example.dhttp.net";
        let response = fixture
            .app
            .clone()
            .oneshot(request(
                &format!("/std/chat-api/conversations/{name}/messages"),
                Method::POST,
                r#"{"text":"old subject only"}"#,
                Some(fixture.owner.clone()),
            ))
            .await
            .expect("queue old message");
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let message = json(response).await;
        let message_id = message["id"].as_str().expect("message id");

        fixture
            .access
            .delete_contacts(&[name.to_owned()])
            .await
            .expect("remove old subject");
        let new_subject = SubjectId::new(b"replacement-subject".to_vec()).expect("new subject");
        fixture
            .access
            .create_contact(NewContact {
                name: name.to_owned(),
                subject_id: new_subject.clone(),
                class: String::from("Human"),
                description: String::from("Replacement"),
                requested_access: crate::chat::CHAT_CAPABILITY.requested_access(),
                offers: Default::default(),
                created_at: 1_700_000_001,
                updated_at: 1_700_000_001,
                expired_after: 2_000_000_000,
            })
            .await
            .expect("new contact identity");
        fixture
            .access
            .patch_contact(
                name,
                ContactPatch::LocalUpdate {
                    status: Some(ContactStatus::Active),
                    alias: None,
                },
            )
            .await
            .expect("activate replacement");
        let capability = fixture
            .app
            .clone()
            .oneshot(request(
                &format!("/std/chat-api/conversations/{name}/capability"),
                Method::GET,
                "",
                Some(fixture.owner.clone()),
            ))
            .await
            .expect("replacement capability");
        assert_eq!(
            json(capability).await["remote_grant"],
            serde_json::Value::Null
        );
        assert!(
            fixture
                .state
                .update_remote_chat_grant(
                    name,
                    &SubjectId::new(b"friend-subject".to_vec()).expect("old subject"),
                    &crate::chat::CHAT_CAPABILITY.offers(),
                )
                .await
                .is_err()
        );

        let shutdown = CancellationToken::new();
        let notify = Arc::new(Notify::new());
        let worker = crate::chat::worker::spawn(
            Arc::downgrade(&fixture.state),
            notify.clone(),
            shutdown.clone(),
        );
        notify.notify_one();
        let listed = wait_for_state(&fixture, "blocked").await;
        assert_eq!(
            listed["items"][0]["error_message"],
            "contact identity changed since message was queued"
        );
        assert!(fixture.requests.lock().expect("remote requests").is_empty());
        let retry = fixture
            .app
            .clone()
            .oneshot(request(
                &format!("/std/chat-api/conversations/{name}/messages/{message_id}/requeue"),
                Method::POST,
                "",
                Some(fixture.owner.clone()),
            ))
            .await
            .expect("retry old message");
        assert_eq!(retry.status(), StatusCode::CONFLICT);
        fixture
            .state
            .update_remote_chat_grant(name, &new_subject, &crate::chat::CHAT_CAPABILITY.offers())
            .await
            .expect("new subject grant");
        let still_blocked = fixture
            .app
            .clone()
            .oneshot(request(
                &format!("/std/chat-api/conversations/{name}/messages"),
                Method::GET,
                "",
                Some(fixture.owner.clone()),
            ))
            .await
            .expect("old message after new grant");
        assert_eq!(json(still_blocked).await["items"][0]["state"], "blocked");
        shutdown.cancel();
        worker.await.expect("stop replacement worker");
        drop(fixture.access);
        std::fs::remove_dir_all(fixture.root).expect("remove fixture");
    }

    #[tokio::test]
    async fn changed_tls_peer_identity_blocks_delivery() {
        let fixture = fixture().await;
        fixture.identity_changed.store(true, Ordering::SeqCst);
        let response = fixture
            .app
            .clone()
            .oneshot(request(
                "/std/chat-api/conversations/friend.example.dhttp.net/messages",
                Method::POST,
                r#"{"text":"do not deliver to replacement"}"#,
                Some(fixture.owner.clone()),
            ))
            .await
            .expect("send response");
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let listed = wait_for_state(&fixture, "blocked").await;
        assert_eq!(
            listed["items"][0]["error_message"],
            crate::chat::outbound::REMOTE_IDENTITY_CHANGED
        );
        assert!(fixture.requests.lock().expect("remote requests").is_empty());
        drop(fixture.access);
        std::fs::remove_dir_all(fixture.root).expect("remove fixture");
    }
}
