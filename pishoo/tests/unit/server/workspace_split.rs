//! Real user Lib acceptance, using private WASI SQLite and native resource APIs.
use std::path::PathBuf;

use dhttp::resolve::{EndpointAddr, Family, Resolve, ResolveFuture, Source};
use futures::{FutureExt, StreamExt};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tower::ServiceExt;

use super::*;
use crate::package::{
    install_lib, remove_lib,
    tests::{package_fixture, publisher},
};

#[derive(Debug)]
struct Peer(EndpointAddr);
impl std::fmt::Display for Peer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Profile and Contact peer")
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
#[ignore = "build profile-lib first; run alone, requires OpenSSL/UDP and owns global TLS/DNS/home"]
async fn profile_and_native_contact_real_quic_workflow_persistence_and_uninstall() {
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
    let project = PathBuf::from(
        std::env::var_os("PISHOO_PROFILE_REPO")
            .expect("set PISHOO_PROFILE_REPO to the independent Profile checkout"),
    );
    let bytes = std::fs::read(project.join("dist/lib.wasm")).expect("build profile-lib first");
    let document: Value =
        serde_json::from_slice(&std::fs::read(project.join("openapi.json")).unwrap()).unwrap();
    let id = "dev.dhttp.net/profile".to_owned();
    let prefix = "/std/profile";
    let chat_id = "dev.dhttp.net/message";
    let chat_project = PathBuf::from(
        std::env::var_os("PISHOO_MESSAGE_REPO")
            .expect("set PISHOO_MESSAGE_REPO to the independent Message checkout"),
    );
    let chat_bytes =
        std::fs::read(chat_project.join("dist/lib.wasm")).expect("build message-lib first");
    let mut chat_document: Value =
        serde_json::from_slice(&std::fs::read(chat_project.join("openapi.json")).unwrap()).unwrap();
    chat_document["x-pishoo"] = json!({"kind":"lib","publisher":"dev.dhttp.net","name":"message","dependencies":[],"host":{"private-storage":"Chat data","request-identity":"Chat caller"},"access-groups":{"message":{"title":"1v1聊天","operations":["receiveMessage"]}}});
    let chat_package = package_fixture(
        &chat_document,
        &chat_bytes,
        webc::v3::write::Directory::default(),
        &publisher("dev.dhttp.net", 'a'),
    );
    let mut document = document;
    document["x-pishoo"] = json!({"kind":"lib","publisher":"dev.dhttp.net","name":"profile","dependencies":[],"host":{"private-storage":"Profile data","request-identity":"Profile caller"}});
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
        std::fs::create_dir_all(profile.join("libs")).unwrap();
        crate::setup::load_server_config(&profile).unwrap();
        install_lib(&profile, chat_id, &chat_package, &runtime).unwrap();
        install_lib(&profile, &id, &package, &runtime).unwrap();
        let server = Server::load(profile.clone(), runtime.clone())
            .await
            .unwrap();
        let access = server.access.clone();
        // Contact submission and profile discovery use the host defaults.
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
    let (sandbox, access, _, profile, _, _) = &listeners[0];
    let contact_db = Arc::new(std::sync::Mutex::new(
        crate::contact::database(profile).unwrap(),
    ));
    assert!(
        !sandbox
            .read()
            .unwrap()
            .libs
            .keys()
            .any(|id| id.ends_with("/workspace"))
    );
    assert_eq!(
        call(
            &alice,
            http::Method::GET,
            "alice.dhttp.net",
            "/std/workspace/context",
            Value::Null
        )
        .await
        .0,
        404
    );
    assert_eq!(
        call(
            &alice,
            http::Method::PATCH,
            "alice.dhttp.net",
            prefix,
            json!({"display_name":"Alice profile"})
        )
        .await
        .0,
        200
    );
    let (_, intro) = call(
        &bob,
        http::Method::GET,
        "alice.dhttp.net",
        "/std/profile/context",
        Value::Null,
    )
    .await;
    assert_eq!(intro["display_name"], "Alice profile");
    assert_eq!(intro["viewer_name"], "bob.dhttp.net");
    assert_eq!(intro["is_owner"], false);
    assert_eq!(
        call(
            &bob,
            http::Method::PATCH,
            "alice.dhttp.net",
            prefix,
            json!({"display_name":"Denied"})
        )
        .await
        .0,
        403
    );
    let requests = "/std/contact/local/requests";
    assert_eq!(
        call(
            &bob,
            http::Method::GET,
            "alice.dhttp.net",
            requests,
            Value::Null
        )
        .await
        .0,
        403
    );
    let (status,queued)=call(&alice,http::Method::POST,"alice.dhttp.net",requests,json!({"target_name":"bob.dhttp.net","description":"Friend","requested_capabilities":["message"],"offered_capabilities":["message"]})).await;
    assert_eq!(status, 201, "{queued}");
    assert_eq!(queued["status"], "queued");
    let (_, local) = call(
        &alice,
        http::Method::GET,
        "alice.dhttp.net",
        "/std/contact/bob.dhttp.net",
        Value::Null,
    )
    .await;
    assert_eq!(local["status"], "pending");
    let (_, rules) = call(
        &alice,
        http::Method::GET,
        "alice.dhttp.net",
        "/std/acl/allow",
        Value::Null,
    )
    .await;
    assert_eq!(
        rules["bob.dhttp.net"]["/std/message"]["allow"],
        json!(["POST"])
    );
    crate::contact::contact_jobs(profile, &alice, access, &contact_db)
        .await
        .unwrap();
    let (_, pending) = call(
        &alice,
        http::Method::GET,
        "alice.dhttp.net",
        &format!("{requests}?id={}", queued["id"]),
        Value::Null,
    )
    .await;
    assert_eq!(pending["status"], "pending", "{pending}");
    let (_, incoming) = call(
        &bob,
        http::Method::GET,
        "bob.dhttp.net",
        "/std/contact/alice.dhttp.net",
        Value::Null,
    )
    .await;
    assert_eq!(incoming["offers"]["/std/message"]["allow"], json!(["POST"]));
    // A later revocation stays authoritative through acceptance and polling.
    assert_eq!(
        call(
            &alice,
            http::Method::DELETE,
            "alice.dhttp.net",
            "/std/acl/allow/bob.dhttp.net",
            json!({"api":"/std/message","method":"POST"})
        )
        .await
        .0,
        204
    );
    assert_eq!(
        call(
            &bob,
            http::Method::PATCH,
            "bob.dhttp.net",
            "/std/contact/alice.dhttp.net",
            json!({"status":"active"})
        )
        .await
        .0,
        204
    );
    let db = rusqlite::Connection::open(profile.join("db/contact.db")).unwrap();
    db.execute("UPDATE outbound_contact_requests SET next_attempt_at=0", [])
        .unwrap();
    drop(db);
    crate::contact::contact_jobs(profile, &alice, access, &contact_db)
        .await
        .unwrap();
    let (_, active) = call(
        &alice,
        http::Method::GET,
        "alice.dhttp.net",
        &format!("{requests}?id={}", queued["id"]),
        Value::Null,
    )
    .await;
    assert_eq!(active["status"], "active", "{active}");
    let (_, contact) = call(
        &alice,
        http::Method::GET,
        "alice.dhttp.net",
        "/std/contact/bob.dhttp.net",
        Value::Null,
    )
    .await;
    assert_eq!(contact["status"], "active");
    assert!(
        contact["granted_access"]["/std/message"]["allow"]
            .as_array()
            .is_none_or(|methods| !methods.contains(&json!("POST"))),
        "activation must not authorize Message"
    );
    let saved = "/std/contact/local/saved?name=bob.dhttp.net";
    assert_eq!(
        call(
            &alice,
            http::Method::PUT,
            "alice.dhttp.net",
            saved,
            Value::Null
        )
        .await
        .0,
        204
    );
    assert_eq!(
        call(
            &alice,
            http::Method::GET,
            "alice.dhttp.net",
            "/std/contact/local/saved",
            Value::Null
        )
        .await
        .1,
        json!(["bob.dhttp.net"])
    );
    assert_eq!(
        call(
            &alice,
            http::Method::DELETE,
            "alice.dhttp.net",
            "/std/contact/bob.dhttp.net",
            Value::Null
        )
        .await
        .0,
        204
    );
    assert_eq!(
        call(
            &alice,
            http::Method::GET,
            "alice.dhttp.net",
            "/std/contact/local/saved",
            Value::Null
        )
        .await
        .1,
        json!([])
    );
    let profile_db = profile
        .join("db")
        .join(id.rsplit('/').next().unwrap())
        .join("profile.db");
    let db = rusqlite::Connection::open(&profile_db).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT display_name FROM profile_preferences WHERE id=1",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "Alice profile"
    );
    assert_eq!(db.query_row("SELECT count(*) FROM sqlite_master WHERE name IN ('saved_contacts','outbound_contact_requests','chat_messages')",[],|r|r.get::<_,i64>(0)).unwrap(),0);
    drop(db);
    remove_lib(profile, &id).unwrap();
    sandbox.write().unwrap().libs.remove(&id);
    assert!(profile_db.exists());
    assert!(profile.join("db/contact.db").exists());
    assert!(sandbox.read().unwrap().libs.contains_key(chat_id));
    assert_eq!(
        call(
            &alice,
            http::Method::GET,
            "alice.dhttp.net",
            prefix,
            Value::Null
        )
        .await
        .0,
        404
    );
    assert_eq!(
        call(
            &alice,
            http::Method::GET,
            "alice.dhttp.net",
            requests,
            Value::Null
        )
        .await
        .0,
        200
    );
}
