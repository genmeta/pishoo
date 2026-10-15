//! Real pinned-store installation of a frontend with a shared transitive backend.
use std::collections::BTreeMap;

use dhttp::resolve::{EndpointAddr, Family, Resolve, ResolveFuture, Source};
use futures::{FutureExt, StreamExt};
use tower::ServiceExt;

use super::*;
use crate::sandbox::tests::{package_fixture, publisher};

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
#[ignore = "run alone; owns temporary TLS/DNS/home, needs OpenSSL 3 and loopback UDP"]
async fn store_app_install_downloads_shared_transitive_dependencies_and_preserves_them() {
    let root = tempfile::tempdir().unwrap();
    credentials::generate_with_owner(
        root.path(),
        &[("alice", "alice.dhttp.net"), ("store", "store.dhttp.net")],
        Some((STORE_NAME, STORE_OWNER)),
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
    // Exercise the actual packaging CLI with temporary signer credentials.
    let tool = std::env::var_os("GENMETA_PACK_TEST_BIN").expect("set GENMETA_PACK_TEST_BIN to the current gmutils executable");
    for kind in ["app", "lib"] {
        let input = root.path().join(format!("tool-{kind}"));
        std::fs::create_dir_all(input.join(if kind == "app" { "public" } else { "metadata" }))
            .unwrap();
        let mut config = format!(
            "[package]\nname = \"alice.dhttp.net/tool-{kind}\"\nversion = \"1.0.0\"\ndescription = \"CLI packaging acceptance\"\nlicense = \"MIT\"\n[package.pishoo]\nkind = \"{kind}\"\ntitle = \"Tool acceptance\"\nchangelog = \"First release; no existing data\"\ndependencies = []\n"
        );
        if kind == "app" {
            config.push_str("entry = \"/\"\n[fs]\n\"/public\" = \"public\"\n");
            std::fs::write(input.join("public/index.html"), b"Packaged frontend").unwrap();
        } else {
            config.push_str("host = {}\n[[module]]\nname = \"backend\"\nsource = \"lib.wasm\"\n[fs]\n\"/metadata\" = \"metadata\"\n");
            std::fs::write(
                input.join("lib.wasm"),
                include_bytes!("../../fixtures/wasi-http-read-request-then-respond.wasm"),
            )
            .unwrap();
            std::fs::write(input.join("metadata/openapi.json"),br#"{"openapi":"3.1.0","info":{"title":"API","version":"1.0.0"},"paths":{"/upload":{"post":{"operationId":"upload","summary":"Upload"}}}}"#).unwrap();
        }
        std::fs::write(input.join("wasmer.toml"), config).unwrap();
        let output = input.join(format!("{kind}.webc"));
        let result = std::process::Command::new(&tool)
            .args(["package", "build", "--no-build"])
            .arg(input.join("wasmer.toml"))
            .arg("--output")
            .arg(&output)
            .args(["--id", "alice.dhttp.net"])
            .env(
                "DHTTP_ROOT_CA_PEM",
                std::fs::read_to_string(root.path().join("ca.crt")).unwrap(),
            )
            .output()
            .expect("build gmutils and provide a publisher validation API before this acceptance test");
        assert!(
            result.status.success(),
            "packaging {kind}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let package = read_package(&std::fs::read(&output).unwrap()).unwrap();
        assert_eq!(package_metadata(&package).unwrap()["kind"], kind);
        assert_eq!(package.atoms().len(), usize::from(kind == "lib"));
        if kind == "app" {
            assert!(package_file(&package, "/metadata/openapi.json").is_err());
        }
    }
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
    let client = dhttp::Endpoint::load("alice").await.unwrap();
    let store = dhttp::Endpoint::load("store").await.unwrap();
    let signer = publisher("alice.dhttp.net", 'a');
    let mut files = BTreeMap::new();
    for (kind, name, deps) in [
        ("lib", "chat", vec![]),
        ("lib", "workspace", vec!["chat"]),
        ("app", "workspace", vec!["workspace", "chat"]),
    ] {
        let dependencies=deps.into_iter().map(|name|serde_json::json!({"publisher":"alice.dhttp.net","name":name,"version":"1.0.0"})).collect::<Vec<_>>();
        let document = serde_json::json!({"openapi":"3.1.0","info":{"title":name,"version":"1.0.0","description":"Installation acceptance","license":{"name":"MIT"}},
            "x-pishoo":{"kind":kind,"publisher":"alice.dhttp.net","name":name,"entry":"/","host":{},"dependencies":dependencies},
            "paths":{"/upload":{"post":{"operationId":"upload","summary":"Upload"}}}});
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
    std::fs::create_dir_all(profile.join("libs")).unwrap();
    crate::setup::load_server_config(&profile).unwrap();
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let sandbox = Arc::new(RwLock::new(Sandbox::new(runtime)));
    let installed = install_store_app(
        &profile,
        &client,
        "alice.dhttp.net",
        "workspace",
        "1.0.0",
        &sandbox,
    )
    .await
    .unwrap();
    assert_eq!(installed["kind"], "app");
    assert!(installed.get("openapi").is_none());
    assert_eq!(sandbox.read().unwrap().libs.len(), 2);
    assert_eq!(sandbox.read().unwrap().apps.len(), 1);
    let conn = rusqlite::Connection::open(profile.config_db_path()).unwrap();
    let counts:(i64,i64,i64,i64)=conn.query_row("SELECT (SELECT count(*) FROM libs),(SELECT count(*) FROM apps),(SELECT count(*) FROM lib_dependencies),(SELECT count(*) FROM app_dependencies)",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
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
    install_store_app(
        &profile,
        &client,
        "alice.dhttp.net",
        "workspace",
        "1.0.0",
        &sandbox,
    )
    .await
    .unwrap();
    assert_eq!(
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.ends_with("/chat/versions/1.0.0/lib.webc"))
            .count(),
        1,
        "reuse the exact installed package without another artifact download"
    );
    assert!(super::super::super::remove_lib(&profile, "alice.dhttp.net/chat").is_err());
    super::super::super::remove_app(&profile, "alice.dhttp.net/workspace").unwrap();
    assert!(super::super::super::remove_lib(&profile, "alice.dhttp.net/chat").is_err());
    super::super::super::remove_lib(&profile, "alice.dhttp.net/workspace").unwrap();
    assert!(profile.join("libs/chat/.webc-proof").is_file());
    sandbox.write().unwrap().load_libs(&profile).unwrap();
    assert_eq!(sandbox.read().unwrap().libs.len(), 1);
}
