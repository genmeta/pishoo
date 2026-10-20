//! Real local Lib calls preserve foreground identity and authenticate CLI HTTP.
use std::path::Path;

use http_body_util::BodyExt;
use serde_json::{Value, json};

use super::*;

async fn call(endpoint: &dhttp::Endpoint, path: &str) -> (u16, Value) {
    let response = tokio::time::timeout(
        Duration::from_secs(15),
        endpoint.get(format!("https://alice.dhttp.net{path}").parse().unwrap()),
    )
    .await
    .unwrap()
    .unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({"body":String::from_utf8_lossy(&bytes)})),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "build tests/fixtures/lib-http-probe and profile-lib; run alone, owns TLS/DNS/home and UDP"]
async fn dependent_libs_use_standard_local_http_with_real_caller_identity() {
    let root = tempfile::tempdir().unwrap();
    credentials::generate(
        root.path(),
        &[("alice", "alice.dhttp.net"), ("bob", "bob.dhttp.net")],
    );
    let previous = std::env::var_os("DHTTP_HOME");
    unsafe {
        std::env::set_var("DHTTP_HOME", root.path());
    }
    let _environment = scopeguard::guard(previous, |previous| unsafe {
        match previous {
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
    dhttp::resolve::Resolver::add(Arc::new(PeerResolver(address)));
    let project = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut profile_doc: Value = serde_json::from_slice(
        &std::fs::read(project.join("../std/libs/profile/openapi.json")).unwrap(),
    )
    .unwrap();
    let publisher = "alice.dhttp.net";
    profile_doc["x-pishoo"] = json!({"kind":"lib","publisher":publisher,"name":"profile",
        "host":{"request-identity":"Profile caller","private-storage":"Profile data"},
        "dependencies":[]});
    let profile_id = format!("{publisher}/profile");
    let probe_id = format!("{publisher}/http-probe");
    let authority = crate::package::tests::publisher(publisher, 'a');
    let profile_package = crate::package::tests::package_fixture(
        &profile_doc,
        &std::fs::read(project.join("../std/libs/profile/dist/lib.wasm")).unwrap(),
        webc::v3::write::Directory::default(),
        &authority,
    );
    let probe_path = std::env::var_os("PISHOO_LIB_HTTP_PROBE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            project.join(
                "tests/fixtures/lib-http-probe/target/wasm32-wasip2/release/lib_http_probe.wasm",
            )
        });
    let probe_doc = json!({"openapi":"3.1.0","info":{"title":"HTTP probe","description":"Probe local Lib calls","version":"0.1.0","license":{"name":"MIT"}},"x-pishoo":{"kind":"lib","publisher":publisher,"name":"http-probe","host":{"request-identity":"Record real caller facts","private-storage":"Persist CLI response"},"dependencies":[{"publisher":publisher,"name":"profile","version":profile_doc["info"]["version"]}]},"paths":{"/call":{"get":{"operationId":"call","summary":"Call dependency"}}}});
    let probe = crate::package::tests::package_fixture(
        &probe_doc,
        &std::fs::read(probe_path).expect("build lib-http-probe first"),
        webc::v3::write::Directory::default(),
        &authority,
    );
    let mut chat_doc: Value = serde_json::from_slice(
        &std::fs::read(project.join("../std/libs/message/openapi.json")).unwrap(),
    )
    .unwrap();
    chat_doc["x-pishoo"] = json!({"kind":"lib","publisher":publisher,"name":"message","host":{"request-identity":"Chat caller","private-storage":"Chat data"},"dependencies":[],"access-groups":{"message":{"title":"1v1聊天","operations":["receiveMessage"]}}});
    let chat = crate::package::tests::package_fixture(
        &chat_doc,
        &std::fs::read(project.join("../std/libs/message/dist/lib.wasm"))
            .expect("build message-lib first"),
        webc::v3::write::Directory::default(),
        &authority,
    );
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.join("libs")).unwrap();
    load_server_config(&profile).unwrap();
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    crate::package::install_lib(&profile, &format!("{publisher}/message"), &chat, &runtime)
        .unwrap();
    crate::package::install_lib(&profile, &profile_id, &profile_package, &runtime).unwrap();
    crate::package::install_lib(&profile, &probe_id, &probe, &runtime).unwrap();
    let mut server = Server::load(profile.clone(), runtime).await.unwrap();
    server
        .access
        .set_policy(
            access_control::Method::Specified(Method::GET),
            "/http-probe/call",
            access_control::Effect::Allow,
            access_control::Grantee::Named,
        )
        .await
        .unwrap();
    server
        .access
        .set_policy(
            access_control::Method::Specified(Method::GET),
            "/profile/profile/context",
            access_control::Effect::Allow,
            access_control::Grantee::All,
        )
        .await
        .unwrap();
    let router = server.router.clone();
    let listener = server
        .endpoint
        .listen(
            dhttp::Scope::Loopback.into(),
            tower::service_fn(move |request: http::Request<Body>| {
                let router = router.read().unwrap().clone();
                async move { router.oneshot(request.map(AxumBody::new)).await }
            }),
        )
        .await
        .unwrap();
    let listener = scopeguard::guard(tokio::spawn(listener), |task| task.abort());
    let alice = server.endpoint.clone();
    let bob = dhttp::Endpoint::load("bob").await.unwrap();
    let (status, owner) = call(&alice, "/http-probe/call").await;
    assert_eq!(status, 200);
    assert_eq!(owner["status"], 200);
    assert_eq!(owner["caller"][0], "alice.dhttp.net");
    assert_eq!(owner["response"]["identity_name"], "alice.dhttp.net");
    let (status, visitor) = call(&bob, "/http-probe/call").await;
    assert_eq!(status, 200);
    assert_eq!(visitor["caller"][0], "bob.dhttp.net");
    assert_eq!(
        visitor["status"], 200,
        "the public context preserves the guest identity"
    );
    assert_eq!(visitor["response"]["viewer_name"], "bob.dhttp.net");
    assert_eq!(visitor["response"]["is_owner"], false);
    crate::sandbox::Sandbox::run(
        server.sandbox.clone(),
        alice,
        server.access.clone(),
        native_router(&server).layer(axum::middleware::from_fn_with_state(
            server.access.clone(),
            authorize,
        )),
    )
    .await;
    let cli: Value = serde_json::from_slice(
        &std::fs::read(
            profile
                .join("db")
                .join(probe_id.rsplit('/').next().unwrap())
                .join("response.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(cli["caller"].is_null());
    assert_eq!(cli["status"], 200);
    assert_eq!(cli["response"]["is_owner"], true);
    assert_eq!(cli["response"]["identity_name"], "alice.dhttp.net");
    server.close().await.unwrap();
    drop(listener);
}
