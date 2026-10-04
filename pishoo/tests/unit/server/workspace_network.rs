//! Explicit real QUIC acceptance test. Run alone: it owns process-wide TLS/DNS/home state.
use std::{
    fmt,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use access_control::{ContactPatch, ContactStatus, SubjectId};
use bytes::Bytes;
use dhttp::resolve::{EndpointAddr, Family, Resolve, ResolveFuture, Source};
use http::{Method, StatusCode};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

use super::*;

#[path = "../../support/network_credentials.rs"]
mod credentials;

#[derive(Debug)]
struct PeerResolver(EndpointAddr);
impl fmt::Display for PeerResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Workspace QUIC acceptance peer")
    }
}
impl Resolve for PeerResolver {
    fn lookup<'a>(&'a self, name: &'a str, _: &'a str, _: Option<Family>) -> ResolveFuture<'a> {
        async move {
            assert!(matches!(
                name,
                "receiver.dhttp.net" | "alice.dhttp.net" | "bob.dhttp.net"
            ));
            Ok(futures::stream::iter([(Source::System, self.0)]).boxed())
        }
        .boxed()
    }
}

fn profile(root: &std::path::Path, name: &str) -> IdentityProfile {
    let profile = IdentityProfile::try_from(root.join(name)).unwrap();
    std::fs::create_dir_all(profile.db_dir()).unwrap();
    let db = rusqlite::Connection::open(profile.config_db_path()).unwrap();
    db.execute_batch("PRAGMA user_version=1; CREATE TABLE settings(listen INTEGER, exec INTEGER); INSERT INTO settings VALUES(0,0); CREATE TABLE proxy_locations(location TEXT,proxy_pass TEXT);").unwrap();
    profile
}

async fn owner_request(
    server: &Server,
    method: Method,
    path: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let request = Request::builder()
        .method(method)
        .uri(format!("https://{}{path}", server.name()))
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(dhttp::WndBuf::with_initial(
            64 * 1024,
            Bytes::from(serde_json::to_vec(&body).unwrap()),
        ))
        .unwrap();
    let (mut writer, response) = server.endpoint.from_request(request).await.unwrap();
    use tokio::io::AsyncWriteExt;
    writer.shutdown().await.unwrap();
    let response = response.await.unwrap();
    assert_eq!(response.version(), http::Version::HTTP_3);
    let status = response.status();
    let bytes = axum::body::to_bytes(AxumBody::new(response.into_body()), 1024 * 1024)
        .await
        .unwrap();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| serde_json::json!({"body": String::from_utf8_lossy(&bytes)}))
    };
    (status, json)
}

#[tokio::test]
#[ignore = "run alone; requires OpenSSL/local UDP and owns global TLS/DNS/DHTTP_HOME"]
async fn config_api_real_h3_persistence_authorization_and_reload() {
    let root = tempfile::tempdir().unwrap();
    credentials::generate(
        root.path(),
        &[
            ("receiver", "receiver.dhttp.net"),
            ("alice", "alice.dhttp.net"),
        ],
    );
    let old_home = std::env::var_os("DHTTP_HOME");
    unsafe {
        std::env::set_var("DHTTP_HOME", root.path());
    }
    let _environment = scopeguard::guard(old_home, |previous| unsafe {
        match previous {
            Some(value) => std::env::set_var("DHTTP_HOME", value),
            None => std::env::remove_var("DHTTP_HOME"),
        }
    });
    dhttp::DhttpNetwork::init().await.unwrap();
    qtls::RootCerts::set([dhttp::CertificateDer::from(
        std::fs::read(root.path().join("ca.der")).unwrap(),
    )])
    .unwrap();
    let peer = Arc::new(qprotocol::UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap());
    let address = EndpointAddr::direct(peer.local_addr().unwrap());
    qprotocol::Dock::global()
        .add(peer.clone())
        .unwrap()
        .unwrap();
    let _socket = scopeguard::guard(peer, |peer| {
        qprotocol::Dock::global().remove(&peer);
    });
    dhttp::resolve::Resolver::add(Arc::new(PeerResolver(address)));
    let mut server = Server::load(
        profile(root.path(), "receiver"),
        Arc::new(WasmRuntime::new().unwrap()),
    )
    .await
    .unwrap();
    let app = server.router.clone();
    let listener = server
        .endpoint
        .listen(
            dhttp::Scope::Loopback.into(),
            tower::service_fn(move |request: Request<Body>| {
                let app = app.read().unwrap().clone();
                async move { app.oneshot(request.map(AxumBody::new)).await }
            }),
        )
        .await
        .unwrap();
    let _listener = scopeguard::guard(tokio::spawn(listener), |task| task.abort());

    let (status, settings) = owner_request(
        &server,
        Method::GET,
        "/sys/settings",
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{settings}");
    assert_eq!(settings, serde_json::json!({"listen":0,"exec":false}));
    let (status, settings) = owner_request(
        &server,
        Method::PATCH,
        "/sys/settings",
        serde_json::json!({"exec":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{settings}");
    assert_eq!(settings, serde_json::json!({"listen":0,"exec":true}));
    assert!(
        crate::setup::load_server_config(&server.profile)
            .unwrap()
            .exec
    );
    assert!(
        !server.config.exec,
        "writing settings does not enable running exec"
    );
    assert!(
        server.reload().await.is_err(),
        "exec changes require restart"
    );
    let (status, _) = owner_request(
        &server,
        Method::PATCH,
        "/sys/settings",
        serde_json::json!({"exec":false}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let proxies =
        serde_json::json!([{"location":"/service/","proxy_pass":"http://127.0.0.1:8080/"}]);
    let (status, saved) =
        owner_request(&server, Method::PUT, "/sys/proxies", proxies.clone()).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved, proxies);
    assert!(
        server.config.proxy_locations.is_empty(),
        "writing proxies awaits reload"
    );
    server.reload().await.unwrap();
    assert_eq!(server.config.proxy_locations[0].location, "/service/");
    let (status, saved) = owner_request(
        &server,
        Method::GET,
        "/sys/proxies",
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved, proxies);
    let (status, _) = owner_request(
        &server,
        Method::PUT,
        "/sys/proxies",
        serde_json::json!([{"location":"/sys/settings","proxy_pass":"127.0.0.1:8080"}]),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        owner_request(
            &server,
            Method::GET,
            "/sys/proxies",
            serde_json::Value::Null
        )
        .await
        .1,
        proxies
    );

    // Even an explicit daccess allow cannot make another TLS identity the profile owner.
    server
        .access
        .set_policy(
            access_control::Method::Unspecified,
            "/sys",
            access_control::Effect::Allow,
            access_control::Grantee::All,
        )
        .await
        .unwrap();
    let alice = dhttp::Endpoint::load("alice").await.unwrap();
    let response = alice
        .get(
            format!("https://{}/sys/settings", server.name())
                .parse()
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.version(), http::Version::HTTP_3);
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = dhttp::Anonymous
        .get(
            format!("https://{}/sys/settings", server.name())
                .parse()
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // Rules still govern the owner, as on existing management endpoints.
    server
        .access
        .set_policy(
            access_control::Method::Unspecified,
            "/sys",
            access_control::Effect::Deny,
            access_control::Grantee::One(server.name().to_owned()),
        )
        .await
        .unwrap();
    let (status, _) = owner_request(
        &server,
        Method::GET,
        "/sys/settings",
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    server.close().await.unwrap();
}

async fn wait_contact(server: &Server, id: i64, expected: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let (status, body) = owner_request(
                server,
                Method::GET,
                &format!("/workspace-api/contact-requests/{id}"),
                serde_json::Value::Null,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            if body["status"] == expected {
                break;
            }
            assert_ne!(body["status"], "failed", "{body}");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("contact worker should deliver and poll over QUIC");
}

async fn wait_message(server: &Server, recipient: &str, id: &str, expected: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let (status, page) = owner_request(
                server,
                Method::GET,
                &format!("/chat-api/conversations/{recipient}/messages"),
                serde_json::Value::Null,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{page}");
            if let Some(message) = page["items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["id"] == id)
            {
                if message["state"] == expected {
                    break;
                }
                assert_ne!(message["state"], "failed", "{message}");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("Chat worker should reach the expected delivery state");
}

#[tokio::test]
#[ignore = "run alone; requires OpenSSL/local UDP and owns global TLS/DNS/DHTTP_HOME"]
async fn workspace_chat_real_quic_delivery_and_pre_send_identity_check() {
    let root = tempfile::tempdir().unwrap();
    credentials::generate(
        root.path(),
        &[
            ("receiver", "receiver.dhttp.net"),
            ("alice", "alice.dhttp.net"),
            ("bob", "bob.dhttp.net"),
        ],
    );
    let old_home = std::env::var_os("DHTTP_HOME");
    unsafe {
        std::env::set_var("DHTTP_HOME", root.path());
    }
    let _environment = scopeguard::guard(old_home, |previous| unsafe {
        match previous {
            Some(value) => std::env::set_var("DHTTP_HOME", value),
            None => std::env::remove_var("DHTTP_HOME"),
        }
    });
    dhttp::DhttpNetwork::init().await.unwrap();
    qtls::RootCerts::set([dhttp::CertificateDer::from(
        std::fs::read(root.path().join("ca.der")).unwrap(),
    )])
    .unwrap();
    let peer = Arc::new(qprotocol::UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap());
    let address = EndpointAddr::direct(peer.local_addr().unwrap());
    qprotocol::Dock::global()
        .add(peer.clone())
        .unwrap()
        .unwrap();
    let _socket = scopeguard::guard(peer, |peer| {
        qprotocol::Dock::global().remove(&peer);
    });
    dhttp::resolve::Resolver::add(Arc::new(PeerResolver(address)));
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let mut receiver = Server::load(profile(root.path(), "receiver"), runtime.clone())
        .await
        .unwrap();
    let mut alice = Server::load(profile(root.path(), "alice"), runtime.clone())
        .await
        .unwrap();
    let mut bob = Server::load(profile(root.path(), "bob"), runtime)
        .await
        .unwrap();
    let probes = Arc::new(AtomicUsize::new(0));
    let messages = Arc::new(AtomicUsize::new(0));
    let app = receiver.router.clone();
    let service = tower::service_fn({
        let probes = probes.clone();
        let messages = messages.clone();
        move |request: Request<Body>| {
            let app = app.read().unwrap().clone();
            if request.headers().contains_key("x-pin-probe") {
                probes.fetch_add(1, Ordering::SeqCst);
            }
            if request.uri().path() == "/std/message" {
                messages.fetch_add(1, Ordering::SeqCst);
            }
            async move { app.oneshot(request.map(AxumBody::new)).await }
        }
    });
    // Use the production Server router with a dedicated loopback registration; listen=0 skips DNS publication.
    let listener = receiver
        .endpoint
        .listen(dhttp::Scope::Loopback.into(), service)
        .await
        .unwrap();
    let listening = tokio::spawn(listener);
    let _listener = scopeguard::guard(listening, |task| task.abort());
    let mut local_listeners = Vec::new();
    for server in [&alice, &bob] {
        let app = server.router.clone();
        let listener = server
            .endpoint
            .listen(
                dhttp::Scope::Loopback.into(),
                tower::service_fn(move |request: Request<Body>| {
                    let app = app.read().unwrap().clone();
                    async move { app.oneshot(request.map(AxumBody::new)).await }
                }),
            )
            .await
            .unwrap();
        local_listeners.push(tokio::spawn(listener));
    }
    let _local_listeners = scopeguard::guard(local_listeners, |tasks| {
        for task in tasks {
            task.abort();
        }
    });
    let (status, body) = owner_request(
        &receiver,
        Method::PATCH,
        "/workspace-api/settings/profile",
        serde_json::json!({"display_name": "Receiver"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Contact admission remains governed by the current daccess policy.
    let denied = crate::workspace::outbound::OutboundTransport::request(
        &alice.endpoint,
        "receiver.dhttp.net",
        Method::POST,
        "/contact",
        Bytes::from_static(b"{}"),
    )
    .await
    .unwrap();
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    receiver
        .access
        .set_policy(
            access_control::Method::Specified(Method::POST),
            "/contact",
            access_control::Effect::Allow,
            access_control::Grantee::All,
        )
        .await
        .unwrap();

    for sender in [&alice, &bob] {
        let (status, body) = owner_request(
            sender,
            Method::GET,
            "/workspace-api/profiles/receiver.dhttp.net",
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["display_name"], "Receiver");
        let (status, created) = owner_request(sender, Method::POST, "/workspace-api/contact-requests", serde_json::json!({
            "target_name": "receiver.dhttp.net", "description": "Hello over QUIC", "requested_capabilities": ["chat"], "offered_capabilities": ["chat"]
        })).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{created}");
        let id = created["id"].as_i64().unwrap();
        wait_contact(sender, id, "pending").await;
        let contact = receiver
            .access
            .find_contact_by_name(sender.name())
            .await
            .unwrap();
        let sender_subject =
            crate::workspace::outbound::OutboundTransport::sender_subject_id(&sender.endpoint)
                .unwrap()
                .unwrap();
        assert_eq!(contact.subject_id.as_bytes(), sender_subject);
        receiver
            .access
            .patch_contact(
                sender.name(),
                ContactPatch::LocalUpdate {
                    status: Some(ContactStatus::Active),
                    alias: None,
                },
            )
            .await
            .unwrap();
        let (status, body) = owner_request(
            &receiver,
            Method::POST,
            &format!(
                "/workspace-api/contacts/{}/capabilities/chat/grant",
                sender.name()
            ),
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
        let (status, body) = owner_request(
            sender,
            Method::POST,
            &format!("/workspace-api/contact-requests/{id}/refresh"),
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        wait_contact(sender, id, "active").await;
        let (status, body) = owner_request(
            sender,
            Method::GET,
            "/chat-api/conversations/receiver.dhttp.net/capability",
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["remote_grant"], true);
        let (status, queued) = owner_request(
            sender,
            Method::POST,
            "/chat-api/conversations/receiver.dhttp.net/messages",
            serde_json::json!({"text": format!("hello from {}", sender.name())}),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{queued}");
        wait_message(
            sender,
            "receiver.dhttp.net",
            queued["id"].as_str().unwrap(),
            "sent",
        )
        .await;
        let (status, page) = owner_request(
            &receiver,
            Method::GET,
            &format!("/chat-api/conversations/{}/messages", sender.name()),
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert_eq!(page["items"].as_array().unwrap().len(), 1);
        assert_eq!(
            page["items"][0]["text"],
            format!("hello from {}", sender.name())
        );
    }
    assert_eq!(messages.load(Ordering::SeqCst), 2);
    // Both Empty and WndBuf must reject a different owner before transmitting headers or bytes, including on a pooled connection.
    let wrong_hash = dhttp_home::certificate::OwnerHash::try_from("0".repeat(64).as_str()).unwrap();
    let result = alice
        .endpoint
        .get("https://receiver.dhttp.net/std/profile".parse().unwrap())
        .expect_remote_owner_hash(wrong_hash.clone())
        .header(
            http::HeaderName::from_static("x-pin-probe"),
            http::HeaderValue::from_static("empty"),
        )
        .await;
    assert!(matches!(result, Err(dhttp::Error::RemoteIdentityChanged)));
    let result = alice
        .endpoint
        .post("https://receiver.dhttp.net/std/message".parse().unwrap())
        .expect_remote_owner_hash(wrong_hash.clone())
        .write(b"must never reach the recipient")
        .header(
            http::HeaderName::from_static("x-pin-probe"),
            http::HeaderValue::from_static("streaming"),
        )
        .await;
    assert!(matches!(result, Err(dhttp::Error::RemoteIdentityChanged)));
    assert_eq!(probes.load(Ordering::SeqCst), 0);
    assert_eq!(messages.load(Ordering::SeqCst), 2);
    // Drive the actual Chat worker with a stale contact pin; it must block and clear cached authorization.
    let wrong_subject = SubjectId::new(wrong_hash.as_str().as_bytes()).unwrap();
    alice
        .access
        .database()
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE contacts SET subject_id = ? WHERE name = ?",
            [
                wrong_subject.as_bytes().to_vec().into(),
                "receiver.dhttp.net".into(),
            ],
        ))
        .await
        .unwrap();
    let contact = alice
        .access
        .find_contact_by_name("receiver.dhttp.net")
        .await
        .unwrap();
    assert_eq!(contact.subject_id, wrong_subject);
    alice
        .chat
        .update_remote_chat_grant(
            "receiver.dhttp.net",
            &wrong_subject,
            &contact.granted_access,
        )
        .await
        .unwrap();
    let (status, queued) = owner_request(
        &alice,
        Method::POST,
        "/chat-api/conversations/receiver.dhttp.net/messages",
        serde_json::json!({"text": "blocked by stale pin"}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{queued}");
    wait_message(
        &alice,
        "receiver.dhttp.net",
        queued["id"].as_str().unwrap(),
        "blocked",
    )
    .await;
    let (_, capability) = owner_request(
        &alice,
        Method::GET,
        "/chat-api/conversations/receiver.dhttp.net/capability",
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(capability["remote_grant"], false);
    assert_eq!(messages.load(Ordering::SeqCst), 2);
    receiver.close().await.unwrap();
    alice.close().await.unwrap();
    bob.close().await.unwrap();
}
