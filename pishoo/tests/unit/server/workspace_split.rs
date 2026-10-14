//! Real user Lib acceptance, using private WASI SQLite and native resource APIs.
use super::*;
use crate::sandbox::tests::{package_fixture, publisher};
use crate::sandbox::{install_lib, remove_lib};
use dhttp::resolve::{EndpointAddr, Family, Resolve, ResolveFuture, Source};
use futures::{FutureExt, StreamExt};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::path::Path;
use tokio::io::AsyncWriteExt;
use tower::ServiceExt;

#[derive(Debug)]
struct Peer(EndpointAddr);
impl std::fmt::Display for Peer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Workspace Lib peer")
    }
}
impl Resolve for Peer {
    fn lookup<'a>(&'a self, _: &'a str, _: &'a str, _: Option<Family>) -> ResolveFuture<'a> {
        async move { Ok(futures::stream::iter([(Source::System, self.0)]).boxed()) }.boxed()
    }
}
async fn call(
    endpoint: &dhttp::Endpoint,
    method: http::Method,
    host: &str,
    path: &str,
    payload: Value,
) -> (u16, Value) {
    let bytes = if payload.is_null() {
        Vec::new()
    } else {
        serde_json::to_vec(&payload).unwrap()
    };
    let request = http::Request::builder()
        .method(method)
        .uri(format!("https://{host}{path}"))
        .header("content-type", "application/json")
        .body(dhttp::WndBuf::new(64 * 1024))
        .unwrap();
    let (mut writer, response) = endpoint.from_request(request).await.unwrap();
    writer.write_all(&bytes).await.unwrap();
    writer.shutdown().await.unwrap();
    let response = tokio::time::timeout(Duration::from_secs(10), response)
        .await
        .unwrap()
        .unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({"body":String::from_utf8_lossy(&bytes)}))
    };
    (status, body)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "build workspace-lib first; run alone, requires OpenSSL/UDP and owns global TLS/DNS/home"]
async fn workspace_user_lib_real_quic_workflow_persistence_and_uninstall() {
    let root = tempfile::tempdir().unwrap();
    credentials::generate(
        root.path(),
        &[("alice", "alice.dhttp.net"), ("bob", "bob.dhttp.net")],
    );
    let previous = std::env::var_os("DHTTP_HOME");
    unsafe {
        std::env::set_var("DHTTP_HOME", root.path());
    }
    let _environment = scopeguard::guard(previous, |value| unsafe {
        match value {
            Some(value) => std::env::set_var("DHTTP_HOME", value),
            None => std::env::remove_var("DHTTP_HOME"),
        }
    });
    dhttp::DhttpNetwork::init().await.unwrap();
    qtls::RootCerts::set([qtls::CertificateDer::from(
        std::fs::read(root.path().join("ca.der")).unwrap(),
    )])
    .unwrap();
    let socket = Arc::new(qprotocol::UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap());
    let address = EndpointAddr::direct(socket.local_addr().unwrap());
    qprotocol::Dock::global()
        .add(socket.clone())
        .unwrap()
        .unwrap();
    let _socket = scopeguard::guard(socket, |socket| {
        qprotocol::Dock::global().remove(&socket);
    });
    dhttp::resolve::Resolver::add(Arc::new(Peer(address)));
    let alice = dhttp::Endpoint::load("alice").await.unwrap();
    let bob = dhttp::Endpoint::load("bob").await.unwrap();
    let project = Path::new(env!("CARGO_MANIFEST_DIR")).join("workspace-lib");
    let bytes = std::fs::read(project.join("target/wasm32-wasip2/release/workspace.wasm"))
        .expect("build workspace-lib first");
    let document: Value =
        serde_json::from_slice(&std::fs::read(project.join("openapi.json")).unwrap()).unwrap();
    let id = "dev.dhttp.net/workspace".to_owned();
    let prefix = "/api/workspace";
    let chat_id = "dev.dhttp.net/chat";
    let chat_project = Path::new(env!("CARGO_MANIFEST_DIR")).join("chat-lib");
    let chat_bytes =
        std::fs::read(chat_project.join("dist/lib.wasm")).expect("build chat-lib first");
    let mut chat_document: Value =
        serde_json::from_slice(&std::fs::read(chat_project.join("openapi.json")).unwrap()).unwrap();
    chat_document["x-pishoo"] = json!({"kind":"lib","publisher":"dev.dhttp.net","name":"chat","dependencies":[],"host":{"private-storage":"Chat data","request-identity":"Chat caller"}});
    let chat_package = package_fixture(
        &chat_document,
        &chat_bytes,
        webc::v3::write::Directory::default(),
        &publisher("dev.dhttp.net", 'a'),
    );
    let mut document = document;
    document["x-pishoo"] = json!({"kind":"lib","publisher":"dev.dhttp.net","name":"workspace","dependencies":[{"publisher":"dev.dhttp.net","name":"chat","version":"0.2.0"}],"host":{"private-storage":"Workspace data","request-identity":"Workspace caller"}});
    let package = package_fixture(
        &document,
        &bytes,
        webc::v3::write::Directory::default(),
        &publisher("dev.dhttp.net", 'a'),
    );
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let mut resources = Vec::new();
    for endpoint in [&alice, &bob] {
        let profile = dhttp_home::identity::IdentityProfile::try_from(
            root.path()
                .join(endpoint.name().trim_end_matches(".dhttp.net")),
        )
        .unwrap();
        std::fs::create_dir_all(profile.join("db")).unwrap();
        std::fs::create_dir_all(profile.join("lib")).unwrap();
        crate::setup::load_server_config(&profile).unwrap();
        install_lib(&profile, chat_id, &chat_package, &runtime).unwrap();
        install_lib(&profile, &id, &package, &runtime).unwrap();
        let server = Server::load(profile.clone(), runtime.clone())
            .await
            .unwrap();
        let access = server.access.clone();
        access
            .set_policy(
                access_control::Method::Specified(http::Method::POST),
                "/std/contact",
                access_control::Effect::Allow,
                access_control::Grantee::Named,
            )
            .await
            .unwrap();
        let sandbox = server.sandbox.clone();
        let native = native_router(&server).layer(axum::middleware::from_fn_with_state(
            access.clone(),
            crate::routes::authorize,
        ));
        let api = server.router.read().unwrap().clone();
        let router = server.router.clone();
        let listener = endpoint
            .listen(
                dhttp::Scope::Loopback.into(),
                tower::service_fn(move |request: http::Request<Body>| {
                    let router = router.read().unwrap().clone();
                    async move { router.oneshot(request.map(axum::body::Body::new)).await }
                }),
            )
            .await
            .unwrap();
        let listener = tokio::spawn(listener);
        resources.push((sandbox, access, native, profile, listener, api));
    }
    let listeners = scopeguard::guard(resources, |resources| {
        for (_, _, _, _, listener, _) in resources {
            listener.abort();
        }
    });
    for (endpoint, (sandbox, access, api, _, _, _)) in
        [&alice, &bob].into_iter().zip(listeners.iter())
    {
        Sandbox::run(
            sandbox.clone(),
            endpoint.clone(),
            access.clone(),
            api.clone(),
        )
        .await;
    }
    let (status, context) = call(
        &alice,
        http::Method::GET,
        alice.name(),
        &format!("{prefix}/context"),
        Value::Null,
    )
    .await;
    assert_eq!(status, 200, "{context}");
    assert_eq!(context["owner_name"], alice.name());
    assert_eq!(
        call(
            &bob,
            http::Method::GET,
            alice.name(),
            &format!("{prefix}/context"),
            Value::Null
        )
        .await
        .0,
        403
    );
    assert_eq!(
        call(
            &alice,
            http::Method::PATCH,
            alice.name(),
            &format!("{prefix}/settings/profile"),
            json!({"display_name":"Alice Workspace"})
        )
        .await
        .0,
        200
    );
    let (status, profile) = call(
        &bob,
        http::Method::GET,
        alice.name(),
        &format!("{prefix}/profile"),
        Value::Null,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(profile["display_name"], "Alice Workspace");
    let image = include_bytes!("../../../../assets/pishoo/pishoo-icon.jpg");
    let request = http::Request::builder()
        .method("PUT")
        .uri(format!(
            "https://{}{prefix}/settings/profile/avatar",
            alice.name()
        ))
        .header("content-type", "image/jpeg")
        .body(dhttp::WndBuf::new(64 * 1024))
        .unwrap();
    let (mut upload, response) = alice.from_request(request).await.unwrap();
    upload.write_all(image).await.unwrap();
    upload.shutdown().await.unwrap();
    let response = response.await.unwrap();
    assert_eq!(response.status(), 200);
    let _ = response.into_body().collect().await.unwrap();
    let response = bob
        .get(
            format!("https://{}{prefix}/profile/avatar", alice.name())
                .parse()
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .as_ref(),
        image
    );
    let request = json!({"target_name":bob.name(),"description":"Hello","requested_capabilities":["chat"],"offered_capabilities":["chat"]});
    let (status, queued) = call(
        &alice,
        http::Method::POST,
        alice.name(),
        &format!("{prefix}/contact-requests"),
        request.clone(),
    )
    .await;
    assert_eq!(status, 201, "{queued}");
    let request_id = queued["id"].as_i64().unwrap();
    assert_eq!(
        call(
            &alice,
            http::Method::POST,
            alice.name(),
            &format!("{prefix}/contact-requests"),
            request
        )
        .await
        .0,
        409
    );
    Sandbox::run(
        listeners[0].0.clone(),
        alice.clone(),
        listeners[0].1.clone(),
        listeners[0].2.clone(),
    )
    .await;
    let (status, approvals) = call(
        &bob,
        http::Method::GET,
        bob.name(),
        &format!("{prefix}/approvals"),
        Value::Null,
    )
    .await;
    assert_eq!(status, 200, "{approvals}");
    assert_eq!(approvals["total"], 1);
    let contact_id = approvals["items"][0]["request_id"].as_i64().unwrap();
    let grant = format!(
        "/api/chat/contacts/capabilities/grant?name={}&capability=chat&request_id={contact_id}&capability_version=1",
        alice.name()
    );
    let (status, body) = call(&bob, http::Method::POST, bob.name(), &grant, Value::Null).await;
    assert_eq!(status, 204, "{body}");
    assert_eq!(
        call(
            &alice,
            http::Method::POST,
            alice.name(),
            &format!("{prefix}/contact-requests/refresh?id={request_id}"),
            Value::Null
        )
        .await
        .0,
        200
    );
    Sandbox::run(
        listeners[0].0.clone(),
        alice.clone(),
        listeners[0].1.clone(),
        listeners[0].2.clone(),
    )
    .await;
    let (_, queued) = call(
        &alice,
        http::Method::GET,
        alice.name(),
        &format!("{prefix}/contact-requests?id={request_id}"),
        Value::Null,
    )
    .await;
    assert_eq!(queued["status"], "active", "{queued}");
    let (status, sent) = call(
        &alice,
        http::Method::POST,
        alice.name(),
        &format!("/api/chat/chat/messages?name={}", bob.name()),
        json!({"text":"WASM message ✓"}),
    )
    .await;
    assert_eq!(status, 202, "{sent}");
    // The sending HTTP request is finished: the standard background entry
    // completes delivery with a fresh Store and no browser polling.
    Sandbox::run(
        listeners[0].0.clone(),
        alice.clone(),
        listeners[0].1.clone(),
        listeners[0].2.clone(),
    )
    .await;
    let (_, messages) = call(
        &bob,
        http::Method::GET,
        bob.name(),
        &format!("/api/chat/chat/messages?name={}", alice.name()),
        Value::Null,
    )
    .await;
    assert_eq!(messages["items"][0]["text"], "WASM message ✓", "{messages}");
    let payload = json!({"client_message_id":sent["client_message_id"],"text":sent["text"]});
    assert_eq!(
        call(
            &alice,
            http::Method::POST,
            bob.name(),
            &format!("/api/chat/message"),
            payload
        )
        .await
        .0,
        200
    );
    let (_, messages) = call(
        &bob,
        http::Method::GET,
        bob.name(),
        &format!("/api/chat/chat/messages?name={}", alice.name()),
        Value::Null,
    )
    .await;
    assert_eq!(messages["items"].as_array().unwrap().len(), 1);
    assert_eq!(
        call(
            &alice,
            http::Method::POST,
            bob.name(),
            &format!("/api/chat/message"),
            json!({"client_message_id":sent["client_message_id"],"text":"changed"})
        )
        .await
        .0,
        409
    );
    assert_eq!(
        call(
            &bob,
            http::Method::POST,
            bob.name(),
            &format!(
                "/api/chat/contacts/capabilities/revoke?name={}&capability=chat",
                alice.name()
            ),
            Value::Null
        )
        .await
        .0,
        204
    );
    assert_eq!(
        call(
            &alice,
            http::Method::POST,
            bob.name(),
            &format!("/api/chat/message"),
            json!({"client_message_id":"revoked","text":"must be denied"})
        )
        .await
        .0,
        403
    );
    assert_eq!(
        call(
            &alice,
            http::Method::POST,
            alice.name(),
            &format!("{prefix}/contact-requests/refresh?id={request_id}"),
            Value::Null
        )
        .await
        .0,
        200
    );
    Sandbox::run(
        listeners[0].0.clone(),
        alice.clone(),
        listeners[0].1.clone(),
        listeners[0].2.clone(),
    )
    .await;
    assert_eq!(
        call(
            &alice,
            http::Method::POST,
            alice.name(),
            &format!("/api/chat/chat/messages?name={}", bob.name()),
            json!({"text":"revoked"})
        )
        .await
        .0,
        403
    );
    // Offering chat and requesting chat are independent choices. Native single
    // contact responses have no incoming field, so an explicit active offer
    // must still be grantable through the ordinary owner operation.
    assert_eq!(
        call(
            &alice,
            http::Method::DELETE,
            alice.name(),
            &format!("/std/contact/{}", bob.name()),
            Value::Null
        )
        .await
        .0,
        204
    );
    let subject = dhttp_home::certificate::extract_dhttp_subject_key_identifier(
        bob.local_authority().unwrap().certificates(),
    )
    .unwrap();
    let (status,body)=call(&alice,http::Method::PUT,alice.name(),&format!("/std/contact/{}",bob.name()),
        json!({"subject_id":subject.owner_hash().as_str(),"class":"Human","description":"offer only","status":"active","requested_access":{},"offers":{"/api/chat/message":{"allow":["POST"],"review":[],"deny":[]}}})).await;
    assert!([200, 201].contains(&status), "{status}: {body}");
    let (status, body) = call(
        &alice,
        http::Method::POST,
        alice.name(),
        &format!(
            "/api/chat/contacts/capabilities/grant?name={}&capability=chat",
            bob.name()
        ),
        Value::Null,
    )
    .await;
    assert_eq!(status, 204, "{body}");
    let (status, body) = call(
        &bob,
        http::Method::POST,
        alice.name(),
        "/api/chat/message",
        json!({"client_message_id":"offer-only","text":"Accepted without a reciprocal request"}),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    // Real concurrent Stores cannot create two pending requests for one target.
    let create_path = format!("{prefix}/contact-requests");
    let (a, b) = tokio::join!(
        call(
            &alice,
            http::Method::POST,
            alice.name(),
            &create_path,
            json!({"target_name":"charlie","description":"test","requested_capabilities":[],"offered_capabilities":[]})
        ),
        call(
            &alice,
            http::Method::POST,
            alice.name(),
            &create_path,
            json!({"target_name":"charlie","description":"test","requested_capabilities":[],"offered_capabilities":[]})
        )
    );
    assert_eq!(
        [a.0, b.0].iter().filter(|status| **status == 201).count(),
        1
    );
    assert!(
        [a.0, b.0]
            .iter()
            .all(|status| [201, 409, 503].contains(status)),
        "{a:?} {b:?}"
    );
    let (sandbox, _, _, profile, _, api) = &listeners[0];
    sandbox.write().unwrap().load_libs(profile).unwrap();
    let (_, stored) = call(
        &alice,
        http::Method::GET,
        alice.name(),
        &format!("{prefix}/settings/profile"),
        Value::Null,
    )
    .await;
    assert_eq!(stored["display_name"], "Alice Workspace");
    sandbox.write().unwrap().libs.remove(&id);
    assert!(remove_lib(profile, chat_id).is_err());
    remove_lib(profile, &id).unwrap();
    let workspace_db = profile.join("db").join(&id).join("workspace.db");
    let chat_db = profile.join("db").join(chat_id).join("chat.db");
    assert!(chat_db.is_file());
    assert!(!profile.join("db").join(&id).join("chat.db").exists());
    assert!(
        !profile
            .join("db")
            .join(chat_id)
            .join("workspace.db")
            .exists()
    );
    let workspace_conn = rusqlite::Connection::open(&workspace_db).unwrap();
    let count: i64 = workspace_conn.query_row("SELECT count(*) FROM sqlite_master WHERE name IN ('capability_decisions','capability_decision_events','chat_messages')",[],|r|r.get(0)).unwrap();
    assert_eq!(count, 0);
    let chat_conn = rusqlite::Connection::open(&chat_db).unwrap();
    let count: i64 = chat_conn
        .query_row("SELECT count(*) FROM chat_messages", [], |r| r.get(0))
        .unwrap();
    assert!(count > 0);
    assert!(listeners[0].0.read().unwrap().libs.contains_key(chat_id));
    assert!(profile.join("db").join(&id).join("workspace.db").is_file());
    assert_eq!(
        call(
            &alice,
            http::Method::GET,
            alice.name(),
            &format!("{prefix}/context"),
            Value::Null
        )
        .await
        .0,
        404
    );
    let _ = api;
    for (sandbox, ..) in listeners.iter() {
        sandbox.write().unwrap().close();
    }
}
