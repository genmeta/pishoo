//! Real pinned-store install/update of an App and shared exact dependencies.
use std::collections::BTreeMap;

use dhttp::resolve::{EndpointAddr, Family, Resolve, ResolveFuture, Source};
use futures::{FutureExt, StreamExt};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tower::ServiceExt;

use super::*;
use crate::{
    package::{
        STORE_NAME, STORE_OWNER, package_metadata,
        tests::{package_fixture, publisher},
    },
    test_support::credentials,
};

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
async fn store_app_updates_exact_dependencies_through_the_management_api() {
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
    for version in ["1.0.0", "2.0.0", "3.0.0"] {
        for (name, frontend, dependencies) in [
            ("profile", false, vec![]),
            ("message", false, vec![]),
            ("introduction", true, vec!["profile"]),
            (
                "workspace",
                true,
                vec!["introduction", "profile", "message"],
            ),
            ("other", true, vec!["profile"]),
        ] {
            let dependencies = dependencies
                .into_iter()
                .map(|name| {
                    serde_json::json!({
                        "publisher":"alice.dhttp.net","name":name,"version":version,
                    })
                })
                .collect::<Vec<_>>();
            let document = serde_json::json!({"openapi":"3.1.0","info":{"title":name,"version":version,"description":"Installation acceptance","license":{"name":"MIT"}},
                "x-pishoo":{"kind":if frontend {"app"} else {"lib"},"publisher":"alice.dhttp.net","name":name,"entry":"/","dependencies":dependencies},
                "paths":{"/upload":{"post":{"operationId":"upload","summary":"Upload"}}}});
            let public = if frontend {
                BTreeMap::from([(
                    "index.html".parse().unwrap(),
                    version.as_bytes().to_vec().into(),
                )])
            } else {
                std::collections::BTreeMap::new()
            };
            let bytes = package_fixture(
                &document,
                include_bytes!("../../../tests/fixtures/wasi-http-read-request-then-respond.wasm"),
                public,
                &signer,
            );
            let package = read_package(&bytes).unwrap();
            let mut proof = package_metadata(&package).unwrap();
            let prefix = format!(
                "/store/v1/packages/{}/versions/{version}",
                proof["id"].as_str().unwrap()
            );
            let download = format!("{prefix}/package.waz");
            proof["download_path"] = download.clone().into();
            proof["status"] = "published".into();
            proof["author"] = proof["publisher"].clone();
            proof["author_owner_hash"] = "a".repeat(64).into();
            proof["dependencies"] = dependencies
                .iter()
                .map(|d| {
                    serde_json::json!({
                        "author":d["publisher"],"name":d["name"],"version":d["version"],
                    })
                })
                .collect::<Vec<_>>()
                .into();
            for key in [
                "publisher",
                "kind",
                "has_lib",
                "has_app",
                "entry",
                "host",
                "openapi",
            ] {
                proof.as_object_mut().unwrap().remove(key);
            }
            proof["size"] = bytes.len().into();
            proof["package_sha256"] = format!("{:x}", Sha256::digest(&bytes)).into();
            let signature = (&package["signature.json"], ()).0;
            let envelope: Value = serde_json::from_slice(&signature).unwrap();
            proof["signer_certificates"] = envelope["certificates"].clone();
            files.insert(prefix, serde_json::to_vec(&proof).unwrap());
            files.insert(download, bytes);
        }
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
                            if req.uri().path().ends_with(".waz") {
                                "application/zip"
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
    let request = |name: &str, version: &str| {
        let mut request = http::Request::builder()
            .method(http::Method::POST)
            .uri("/std/sys/pkg/install")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                serde_json::json!({"author":"alice.dhttp.net","name":name,"version":version})
                    .to_string(),
            ))
            .unwrap();
        request
            .extensions_mut()
            .insert(access_control::Visitor::new(client.name(), owner.clone()));
        request
    };
    for version in ["1.0.0", "2.0.0"] {
        let response = management
            .clone()
            .oneshot(request("workspace", version))
            .await
            .unwrap();
        let status = response.status();
        assert_eq!(response.headers()["cache-control"], "no-store");
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(
            status,
            http::StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&body)
        );
        let result: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(result["id"], "alice.dhttp.net/workspace");
        assert_eq!(result["version"], version);
        assert_eq!(result["dependencies"].as_array().unwrap().len(), 3);
        assert_eq!(result["dependencies"][0]["author"], "alice.dhttp.net");
        let loaded = sandbox.read().unwrap();
        assert_eq!(loaded.libs.len(), 2);
        assert_eq!(loaded.apps.len(), 2);
        for name in ["profile", "message"] {
            let lib = &loaded.libs[&format!("alice.dhttp.net/{name}")];
            assert_eq!(lib.openapi.info.version, version);
            assert!(wasmparser::Parser::is_component(
                &std::fs::read(profile.join(format!("lib/{name}.wasm"))).unwrap()
            ));
        }
        assert_eq!(
            std::fs::read(profile.join("app/workspace/index.html")).unwrap(),
            version.as_bytes()
        );
        assert_eq!(
            requests
                .lock()
                .unwrap()
                .iter()
                .filter(|p| p.ends_with(&format!("/profile/versions/{version}/package.waz")))
                .count(),
            1,
            "shared direct/transitive dependency must be downloaded only once"
        );
    }
    let previous = sandbox.read().unwrap().libs["alice.dhttp.net/profile"].clone();
    let response = management
        .clone()
        .oneshot(request("workspace", "2.0.0"))
        .await
        .unwrap();
    assert_eq!(response.status(), http::StatusCode::OK);
    assert!(Arc::ptr_eq(
        &previous,
        &sandbox.read().unwrap().libs["alice.dhttp.net/profile"]
    ));
    let response = management
        .clone()
        .oneshot(request("other", "2.0.0"))
        .await
        .unwrap();
    assert_eq!(response.status(), http::StatusCode::OK);
    let before = crate::package::installed_packages(&profile, None).unwrap();
    let response = management
        .clone()
        .oneshot(request("workspace", "3.0.0"))
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    assert_eq!(
        status,
        http::StatusCode::CONFLICT,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert!(
        String::from_utf8_lossy(&body)
            .contains("alice.dhttp.net/other requires alice.dhttp.net/profile@2.0.0")
    );
    assert_eq!(
        crate::package::installed_packages(&profile, None).unwrap(),
        before
    );
    assert!(Arc::ptr_eq(
        &previous,
        &sandbox.read().unwrap().libs["alice.dhttp.net/profile"]
    ));
    assert_eq!(
        std::fs::read(profile.join("app/workspace/index.html")).unwrap(),
        b"2.0.0"
    );
    let response = management
        .oneshot(request("workspace", "4.0.0"))
        .await
        .unwrap();
    assert_eq!(response.status(), http::StatusCode::NOT_FOUND);
    assert_eq!(
        crate::package::installed_packages(&profile, None).unwrap(),
        before
    );
}
