//! Real pinned-store installation of a frontend with a shared transitive backend.
use std::collections::BTreeMap;

use dhttp::resolve::{EndpointAddr, Family, Resolve, ResolveFuture, Source};
use futures::{FutureExt, StreamExt};
use tower::ServiceExt;

use super::*;
use crate::package::tests::{package_fixture, publisher};
use crate::package::{STORE_NAME, STORE_OWNER, package_metadata};
use serde_json::Value;
use sha2::{Digest, Sha256};

#[allow(dead_code)]
#[path = "../../support/network_credentials.rs"]
mod credentials;

#[derive(Debug)]
struct Peer(EndpointAddr);
impl std::fmt::Display for Peer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("temporary package store")
    }
}
impl Resolve for Peer {
    fn lookup<'a>(&'a self, _: &'a str, _: &'a str, _: Option<Family>) -> ResolveFuture<'a> {
        async move { Ok(futures::stream::iter([(Source::System, self.0)]).boxed()) }.boxed()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "run alone; owns temporary TLS/DNS, needs OpenSSL 3 and loopback UDP"]
async fn store_app_install_downloads_shared_transitive_dependencies_and_preserves_them() {
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter("warn,pishoo=debug,dhttp=debug")
        .with_test_writer()
        .try_init();
    let root = tempfile::tempdir().unwrap();
    credentials::generate_with_owner(
        root.path(),
        &[("alice", "alice.dhttp.net"), ("store", "store.dhttp.net")],
        Some((STORE_NAME, STORE_OWNER)),
    );
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
    qprotocol::AddressBook::global()
        .insert_inner(&socket, address)
        .unwrap();
    let _socket = scopeguard::guard(socket, |socket| {
        qprotocol::Dock::global().remove(&socket);
    });
    // QUIC does not pair an endpoint with itself. Keep a distinct loopback
    // binding for the client, with both sockets owned by this isolated test.
    let outgoing = Arc::new(qprotocol::UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap());
    let outgoing_address = EndpointAddr::direct(outgoing.local_addr().unwrap());
    qprotocol::Dock::global()
        .add(outgoing.clone())
        .unwrap()
        .unwrap();
    qprotocol::AddressBook::global()
        .insert_inner(&outgoing, outgoing_address)
        .unwrap();
    let _outgoing = scopeguard::guard(outgoing, |socket| {
        qprotocol::Dock::global().remove(&socket);
    });
    dhttp::resolve::Resolver::add(Arc::new(Peer(address)));
    let client = dhttp::Endpoint::load_from(root.path().join("alice"))
        .await
        .unwrap();
    let store = dhttp::Endpoint::load_from(root.path().join("store"))
        .await
        .unwrap();
    let signer = publisher("alice.dhttp.net", 'a');
    let mut files = BTreeMap::new();
    for (kind, name, deps) in [
        ("lib", "chat", vec![]),
        ("lib", "workspace", vec!["chat"]),
        ("app", "workspace", vec!["workspace", "chat"]),
        ("lib", "blocked", vec![]),
    ] {
        let dependencies=deps.into_iter().map(|name|serde_json::json!({"publisher":"alice.dhttp.net","name":name,"version":"1.0.0"})).collect::<Vec<_>>();
        let route = if name == "blocked" { "chat" } else { name };
        let document = serde_json::json!({"openapi":"3.1.0","info":{"title":name,"version":"1.0.0","description":"Installation acceptance","license":{"name":"MIT"}},
            "x-pishoo":{"kind":kind,"publisher":"alice.dhttp.net","name":name,"entry":"/","host":{},"dependencies":dependencies},
            "paths":{(format!("/{route}/upload")):{"post":{"operationId":"upload","summary":"Upload"}}}});
        let public = if kind == "app" {
            webc::v3::write::Directory::new(
                BTreeMap::from([("index.html".parse().unwrap(), b"Workspace".to_vec().into())]),
                webc::v3::Timestamps::default(),
            )
        } else {
            webc::v3::write::Directory::default()
        };
        let bytes = package_fixture(
            &document,
            include_bytes!("../../fixtures/wasi-http-read-request-then-respond.wasm"),
            public,
            &signer,
        );
        let package = read_package(&bytes).unwrap();
        let mut proof = package_metadata(&package).unwrap();
        let prefix = format!(
            "/store/v1/{}/{}/versions/1.0.0",
            if kind == "app" { "apps" } else { "libs" },
            proof["id"].as_str().unwrap()
        );
        let download = format!("{prefix}/{kind}.webc");
        proof["download_path"] = download.clone().into();
        proof["status"] = "published".into();
        proof["publisher_owner_hash"] = "a".repeat(64).into();
        proof["size"] = bytes.len().into();
        proof["package_sha256"] = format!("{:x}", Sha256::digest(&bytes)).into();
        let signature = package
            .get_volume("pishoo-signature")
            .unwrap()
            .read_file("/signature.json")
            .unwrap()
            .0;
        let envelope: Value = serde_json::from_slice(&signature).unwrap();
        proof["signer_certificates"] = envelope["certificates"].clone();
        if kind == "lib" {
            proof["openapi"] =
                serde_json::from_slice(&package_file(&package, "/metadata/openapi.json").unwrap())
                    .unwrap();
        }
        files.insert(prefix, serde_json::to_vec(&proof).unwrap());
        files.insert(download, bytes);
    }
    let files = Arc::new(files);
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let app = axum::Router::new().fallback({
        let requests = requests.clone();
        move |req: http::Request<axum::body::Body>| {
            let files = files.clone();
            let requests = requests.clone();
            async move {
                requests.lock().unwrap().push(req.uri().path().to_owned());
                match files.get(req.uri().path()) {
                    Some(bytes) => http::Response::builder()
                        .status(200)
                        .header(
                            "content-type",
                            if req.uri().path().ends_with(".webc") {
                                "application/webc"
                            } else {
                                "application/json"
                            },
                        )
                        .body(axum::body::Body::from(bytes.clone()))
                        .unwrap(),
                    None => http::Response::builder()
                        .status(404)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                }
            }
        }
    });
    let listener = store
        .listen(
            dhttp::Scope::Loopback.into(),
            tower::service_fn(move |req: http::Request<crate::Body>| {
                let app = app.clone();
                async move { app.oneshot(req.map(axum::body::Body::new)).await }
            }),
        )
        .await
        .unwrap();
    let _listener = scopeguard::guard(tokio::spawn(listener), |task| task.abort());
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.join("lib")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let sandbox = Arc::new(RwLock::new(Sandbox::new(runtime)));
    let management =
        crate::package::pkg_management_router(profile.clone(), client.clone(), sandbox.clone());
    let authority = client.local_authority().unwrap();
    let ski =
        dhttp_home::certificate::extract_dhttp_subject_key_identifier(authority.certificates())
            .unwrap();
    let owner = access_control::SubjectId::new(ski.owner_hash().as_str().as_bytes()).unwrap();
    let request = || {
        let mut request = http::Request::builder()
            .method(http::Method::PUT)
            .uri("/std/sys/pkg?name=workspace")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                serde_json::json!({"publisher":"alice.dhttp.net","version":"1.0.0"}).to_string(),
            ))
            .unwrap();
        request
            .extensions_mut()
            .insert(access_control::Visitor::new(client.name(), owner.clone()));
        request
    };
    let response = management.clone().oneshot(request()).await.unwrap();
    assert_eq!(response.status(), http::StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let installed: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(installed["kind"], "app");
    assert!(installed.get("openapi").is_none());
    assert_eq!(sandbox.read().unwrap().libs.len(), 2);
    assert_eq!(sandbox.read().unwrap().apps.len(), 1);
    assert!(
        sandbox.read().unwrap().libs["alice.dhttp.net/chat"]
            .package
            .get_volume("pishoo-signature")
            .is_none()
    );
    assert!(
        sandbox.read().unwrap().apps["alice.dhttp.net/workspace"]
            .get_volume("pishoo-signature")
            .is_none()
    );
    let conn =
        crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    let counts:(i64,i64,i64,i64)=conn.query_row("SELECT (SELECT count(*) FROM lib),(SELECT count(*) FROM app),(SELECT count(*) FROM lib_dependency),(SELECT count(*) FROM app_dependency)",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
    assert_eq!(counts, (2, 1, 1, 2));
    assert_eq!(
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.ends_with("/chat/versions/1.0.0/lib.webc"))
            .count(),
        1
    );
    let before = conn
        .prepare("SELECT name,author,version FROM lib ORDER BY name")
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    let edges = conn
        .prepare("SELECT app_name,lib_name,version FROM app_dependency ORDER BY app_name,lib_name")
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    let response = management.clone().oneshot(request()).await.unwrap();
    assert_eq!(response.status(), http::StatusCode::OK);
    let repeated: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(repeated["id"], installed["id"]);
    assert_eq!(repeated["version"], installed["version"]);
    assert_eq!(repeated["dependencies"], installed["dependencies"]);
    let after = conn
        .prepare("SELECT name,author,version FROM lib ORDER BY name")
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    let repeated_edges = conn
        .prepare("SELECT app_name,lib_name,version FROM app_dependency ORDER BY app_name,lib_name")
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(before, after);
    assert_eq!(edges, repeated_edges);
    assert_eq!(sandbox.read().unwrap().libs.len(), 2);
    assert_eq!(sandbox.read().unwrap().apps.len(), 1);
    assert_eq!(
        conn.query_row("SELECT count(*) FROM app", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        conn.query_row("SELECT count(*) FROM lib_dependency", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|path| path.ends_with("/chat/versions/1.0.0/lib.webc"))
            .count(),
        2
    );

    // This name is unregistered, so the early author/name conflict check cannot
    // reject it. The valid signed input must be downloaded before its route
    // conflicts with chat. Preparing a rejected Lib must not create a flat data
    // directory that would hide another author's existing legacy data later.
    let legacy = profile.join("db/bob.dhttp.net/blocked");
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::write(legacy.join("keep.db"), b"legacy blocked data").unwrap();
    assert!(!profile.join("db/blocked").exists());
    let chat = sandbox.read().unwrap().libs["alice.dhttp.net/chat"].clone();
    let mut rejected = http::Request::builder()
        .method(http::Method::PUT)
        .uri("/std/sys/pkg?name=blocked")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            serde_json::json!({"publisher":"alice.dhttp.net","version":"1.0.0"}).to_string(),
        ))
        .unwrap();
    rejected
        .extensions_mut()
        .insert(access_control::Visitor::new(client.name(), owner.clone()));
    let response = management.clone().oneshot(rejected).await.unwrap();
    let status = response.status();
    let error = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    assert_eq!(
        status,
        http::StatusCode::CONFLICT,
        "{}",
        String::from_utf8_lossy(&error)
    );
    assert_eq!(
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|path| path.ends_with("/blocked/versions/1.0.0/lib.webc"))
            .count(),
        1,
        "the conflict must follow a real signed store download"
    );
    assert!(!profile.join("db/blocked").exists());
    assert!(!profile.join("lib/blocked.wasm").exists());
    assert_eq!(
        std::fs::read(legacy.join("keep.db")).unwrap(),
        b"legacy blocked data"
    );
    assert_eq!(
        conn.query_row("SELECT count(*) FROM lib WHERE name='blocked'", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap(),
        0
    );
    assert_eq!(sandbox.read().unwrap().libs.len(), 2);
    assert!(Arc::ptr_eq(
        &chat,
        &sandbox.read().unwrap().libs["alice.dhttp.net/chat"]
    ));
    assert!(
        !sandbox
            .read()
            .unwrap()
            .libs
            .contains_key("alice.dhttp.net/blocked")
    );
    assert!(crate::package::remove_lib(&profile, "alice.dhttp.net/chat").is_err());
    crate::package::remove_app(&profile, "alice.dhttp.net/workspace").unwrap();
    assert!(crate::package::remove_lib(&profile, "alice.dhttp.net/chat").is_err());
    crate::package::remove_lib(&profile, "alice.dhttp.net/workspace").unwrap();
    assert!(profile.join("lib/chat.wasm").is_file());
    sandbox.write().unwrap().load_libs(&profile).unwrap();
    assert_eq!(sandbox.read().unwrap().libs.len(), 1);
}
