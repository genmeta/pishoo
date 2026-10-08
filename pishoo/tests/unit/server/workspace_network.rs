//! Explicit real QUIC acceptance test. Run alone: it owns process-wide TLS/DNS/home state.
use std::{
    fmt,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use access_control::{ContactPatch, ContactStatus, SubjectId};
use bytes::Bytes;
use dhttp::resolve::{EndpointAddr, Family, Resolve, ResolveFuture, Source};
use futures::FutureExt;
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
    db.execute_batch("PRAGMA user_version=1; CREATE TABLE settings(listen INTEGER); INSERT INTO settings VALUES(0); CREATE TABLE proxy_locations(location TEXT,proxy_pass TEXT);").unwrap();
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
        .body(dhttp::WndBuf::new(64 * 1024))
        .unwrap();
    let (mut writer, response) = server.endpoint.from_request(request).await.unwrap();
    use tokio::io::AsyncWriteExt;
    writer
        .write_all(&serde_json::to_vec(&body).unwrap())
        .await
        .unwrap();
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
#[ignore = "run alone; requires OpenSSL/local TCP+UDP and owns global TLS/DNS/DHTTP_HOME"]
async fn ocsp_refresh_replaces_live_credentials_without_reloading_application() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let root = tempfile::tempdir().unwrap();
    credentials::generate(root.path(), &[("receiver", "receiver.dhttp.net")]);
    let responder = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}/ocsp", responder.local_addr().unwrap());
    let openssl = |args: &[&str]| {
        let output = std::process::Command::new(
            std::env::var_os("DHTTP_TEST_OPENSSL").unwrap_or_else(|| "openssl".into()),
        )
        .args(args)
        .current_dir(root.path())
        .output()
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    let mut extensions = std::fs::read_to_string(root.path().join("leaf.ext")).unwrap();
    extensions.push_str(&format!("authorityInfoAccess=OCSP;URI:{uri}\n"));
    std::fs::write(root.path().join("leaf.ext"), extensions).unwrap();
    openssl(&[
        "x509",
        "-req",
        "-sha256",
        "-in",
        "leaf.csr",
        "-CA",
        "ca.crt",
        "-CAkey",
        "ca.key",
        "-set_serial",
        "01",
        "-out",
        "receiver/ssl/fullchain.crt",
        "-days",
        "1",
        "-extfile",
        "leaf.ext",
    ]);
    let cert_path = root.path().join("receiver/ssl/fullchain.crt");
    let mut chain = std::fs::read(&cert_path).unwrap();
    chain.extend(std::fs::read(root.path().join("ca.crt")).unwrap());
    std::fs::write(cert_path, chain).unwrap();
    let sign_ocsp = |path: &str| {
        openssl(&[
            "ocsp",
            "-rmd",
            "sha256",
            "-index",
            "index.txt",
            "-rsigner",
            "ca.crt",
            "-rkey",
            "ca.key",
            "-CA",
            "ca.crt",
            "-issuer",
            "ca.crt",
            "-cert",
            "receiver/ssl/fullchain.crt",
            "-respout",
            path,
            "-ndays",
            "1",
        ])
    };
    sign_ocsp("receiver/ssl/ocsp.der");
    sign_ocsp("renewed.ocsp");
    let renewed = std::fs::read(root.path().join("renewed.ocsp")).unwrap();
    let before = std::fs::read(root.path().join("receiver/ssl/ocsp.der")).unwrap();
    assert_ne!(renewed, before);

    let response_bytes = renewed.clone();
    let responder_task = tokio::spawn(async move {
        // First return a signed renewal; then a malformed response to exercise retention.
        for body in [response_bytes, b"invalid OCSP".to_vec()] {
            let (mut socket, _) = responder.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut buffer = [0; 4096];
                let length = socket.read(&mut buffer).await.unwrap();
                assert_ne!(length, 0);
                request.extend_from_slice(&buffer[..length]);
                if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    let headers = std::str::from_utf8(&request[..end]).unwrap();
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap();
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            assert!(request.starts_with(b"POST /ocsp "));
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/ocsp-response\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
            socket.write_all(&body).await.unwrap();
            socket.shutdown().await.unwrap();
        }
    });
    let _responder = scopeguard::guard(responder_task, |task| task.abort());
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
    let profile = profile(root.path(), "receiver");
    rusqlite::Connection::open(profile.config_db_path())
        .unwrap()
        .execute("UPDATE settings SET listen=3", [])
        .unwrap();
    let mut server = Server::load(profile, Arc::new(WasmRuntime::new().unwrap()))
        .await
        .unwrap();
    // Keep the in-memory configuration and application resources through the update.
    let workspace = server.workspace.clone();
    let chat = server.chat.clone();
    let router = server.router.clone();
    let publisher = server.publisher.clone().unwrap();
    let app = router.clone();
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
    let (status, _) = owner_request(
        &server,
        Method::GET,
        "/sys/settings",
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // A disk config change must stay unapplied; no identity/Lib reload occurs.
    rusqlite::Connection::open(server.profile.config_db_path())
        .unwrap()
        .execute("UPDATE settings SET listen=1", [])
        .unwrap();

    let endpoint = renew_ocsp(&server.profile, &server.endpoint).await.unwrap();
    apply_ocsp(&mut server, endpoint).await.unwrap();
    assert_eq!(server.endpoint.local_authority().unwrap().ocsp(), renewed);
    assert_eq!(std::fs::read(server.profile.ocsp_path()).unwrap(), renewed);
    assert!(Arc::ptr_eq(&workspace, &server.workspace));
    assert!(Arc::ptr_eq(&chat, &server.chat));
    assert!(Arc::ptr_eq(&router, &server.router));
    assert_eq!(server.config.listen, 3);
    assert!(!Arc::ptr_eq(&publisher, server.publisher.as_ref().unwrap()));
    // The existing pooled connection remains usable after renewal.
    let (status, settings) = owner_request(
        &server,
        Method::GET,
        "/sys/settings",
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{settings}");
    // Anonymous traffic creates a different connection and verifies the renewed TLS staple.
    let request = Request::builder()
        .uri("https://receiver.dhttp.net/std/profile")
        .body(http_body_util::Empty::<Bytes>::new())
        .unwrap();
    let response = dhttp::Request::new(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .extensions()
            .get::<dhttp::RemoteAuthority>()
            .unwrap()
            .name(),
        server.name()
    );
    assert!(renew_ocsp(&server.profile, &server.endpoint).await.is_err());
    assert_eq!(server.endpoint.local_authority().unwrap().ocsp(), renewed);
    assert_eq!(std::fs::read(server.profile.ocsp_path()).unwrap(), renewed);
    server.close().await.unwrap();
}

#[tokio::test]
#[ignore = "run alone; requires OpenSSL and owns global TLS/DHTTP_HOME"]
async fn identity_validation_failure_skips_profile_and_loads_other_identities() {
    let root = tempfile::tempdir().unwrap();
    credentials::generate(
        root.path(),
        &[
            ("valid", "valid.dhttp.net"),
            ("bad-key", "bad-key.dhttp.net"),
            ("bad-config", "bad-config.dhttp.net"),
            ("bad-lib", "bad-lib.dhttp.net"),
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
    qtls::RootCerts::set([dhttp::CertificateDer::from(
        std::fs::read(root.path().join("ca.der")).unwrap(),
    )])
    .unwrap();

    for name in ["missing", "malformed", "expired"] {
        std::fs::create_dir_all(root.path().join(name).join("ssl")).unwrap();
    }
    std::fs::write(root.path().join("malformed/ssl/fullchain.crt"), b"not PEM").unwrap();
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::new(vec!["expired.dhttp.net".into()]).unwrap();
    params.not_before = rcgen::date_time_ymd(2000, 1, 1);
    params.not_after = rcgen::date_time_ymd(2001, 1, 1);
    let expired = params.self_signed(&key).unwrap();
    std::fs::write(
        root.path().join("expired/ssl/fullchain.crt"),
        format!("{}{}", expired.pem(), expired.pem()),
    )
    .unwrap();
    std::fs::write(root.path().join("expired/ssl/ocsp.der"), [1]).unwrap();
    let bad_key = root.path().join("bad-key/ssl/privkey.pem");
    let original_key = std::fs::read(&bad_key).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bad_key, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    std::fs::write(&bad_key, b"not a private key").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bad_key, std::fs::Permissions::from_mode(0o400)).unwrap();
    }

    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let error = Server::load(
        IdentityProfile::try_from(root.path().join("expired")).unwrap(),
        runtime.clone(),
    )
    .await
    .err()
    .expect("expired certificate must fail validation before OCSP fetching");
    assert!(
        error.to_string().contains("outside its validity period"),
        "{error}"
    );
    // Startup skips invalid identities and still loads later valid profiles.
    for name in ["missing", "malformed", "expired", "bad-key"] {
        let identity = IdentityProfile::try_from(root.path().join(name)).unwrap();
        assert!(
            load_profile(identity.clone(), runtime.clone())
                .await
                .unwrap()
                .is_none(),
            "{name} must be skipped"
        );
        assert!(
            !identity.db_dir().exists(),
            "{name} must not be initialized"
        );
    }
    let mut valid = load_profile(profile(root.path(), "valid"), runtime.clone())
        .await
        .unwrap()
        .expect("a valid identity after invalid profiles must load");
    assert_eq!(valid.name(), "valid.dhttp.net");
    valid.close().await.unwrap();

    // A corrected identity must not be remembered as permanently invalid.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bad_key, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    std::fs::write(&bad_key, original_key).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bad_key, std::fs::Permissions::from_mode(0o400)).unwrap();
    }
    let mut repaired = load_profile(profile(root.path(), "bad-key"), runtime.clone())
        .await
        .unwrap()
        .expect("a repaired identity must load on a later scan");
    repaired.close().await.unwrap();

    let config = profile(root.path(), "bad-config");
    rusqlite::Connection::open(config.config_db_path())
        .unwrap()
        .execute_batch("UPDATE settings SET listen=99;")
        .unwrap();
    assert!(matches!(
        load_profile(config, runtime.clone()).await,
        Err(Error::InvalidConfig(_))
    ));
    let lib = profile(root.path(), "bad-lib");
    std::fs::create_dir_all(lib.join("lib/broken")).unwrap();
    std::fs::write(lib.join("lib/broken/lib.wasm"), b"not WASM").unwrap();
    assert!(matches!(
        load_profile(lib, runtime).await,
        Err(Error::InvalidComponent(_))
    ));
}

#[tokio::test]
#[ignore = "run alone; requires OpenSSL/local UDP and owns global TLS/DNS/DHTTP_HOME"]
async fn config_api_real_h3_persistence_authorization_and_restart() {
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
    let listener_app = app.clone();
    let listener = server
        .endpoint
        .listen(
            dhttp::Scope::Loopback.into(),
            tower::service_fn(move |request: Request<Body>| {
                let app = listener_app.read().unwrap().clone();
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
    assert_eq!(settings, serde_json::json!({"listen":0}));
    let (status, settings) = owner_request(
        &server,
        Method::PATCH,
        "/sys/settings",
        serde_json::json!({"listen":1}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{settings}");
    assert_eq!(settings, serde_json::json!({"listen":1}));
    assert_eq!(
        crate::setup::load_server_config(&server.profile)
            .unwrap()
            .listen,
        1
    );
    assert_eq!(server.config.listen, 0, "writing settings requires restart");
    let proxies =
        serde_json::json!([{"location":"/service/","proxy_pass":"http://127.0.0.1:8080/"}]);
    let (status, saved) =
        owner_request(&server, Method::PUT, "/sys/proxies", proxies.clone()).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved, proxies);
    assert!(
        server.config.proxy_locations.is_empty(),
        "writing proxies requires restart"
    );
    server.close().await.unwrap();
    server = Server::load(
        server.profile.clone(),
        Arc::new(WasmRuntime::new().unwrap()),
    )
    .await
    .unwrap();
    assert_eq!(
        server.config.listen, 1,
        "restart loads the saved listen setting"
    );
    assert_eq!(server.config.proxy_locations[0].location, "/service/");
    *app.write().unwrap() = server.router.read().unwrap().clone();
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
    let receiver_profile = profile(root.path(), "receiver");
    std::fs::remove_file(receiver_profile.config_db_path()).unwrap();
    let mut receiver = Server::load(receiver_profile, runtime.clone())
        .await
        .unwrap();
    assert_eq!(receiver.config.listen, 3);
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
    // Use the production Server router with loopback-only registration. The run loop's DNS publication is not started.
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
    let initial = crate::workspace::outbound::OutboundTransport::request(
        &alice.endpoint,
        "receiver.dhttp.net",
        Method::POST,
        "/contact",
        Bytes::from_static(b"{}"),
    )
    .await
    .unwrap();
    assert_eq!(
        initial.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "default named rule reaches the library's JSON validation"
    );
    receiver
        .access
        .set_policy(
            access_control::Method::Specified(Method::POST),
            "/contact",
            access_control::Effect::Deny,
            access_control::Grantee::Named,
        )
        .await
        .unwrap();
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
            access_control::Grantee::Named,
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

        // One application must confirm the receiver's reverse permission too;
        // the receiver never submits a second friend application.
        tokio::time::timeout(Duration::from_secs(40), async {
            loop {
                let (status, capability) = owner_request(
                    &receiver,
                    Method::GET,
                    &format!("/chat-api/conversations/{}/capability", sender.name()),
                    serde_json::Value::Null,
                )
                .await;
                assert_eq!(status, StatusCode::OK, "{capability}");
                if capability["remote_grant"] == true {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("incoming application should confirm the reverse grant");
        let (status, directory) = owner_request(
            &receiver,
            Method::GET,
            "/workspace-api/contact-directory",
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let entry = directory
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == sender.name())
            .unwrap();
        assert_eq!(entry["remote_chat_granted"], true);
        assert_eq!(entry["chat_available"], true);
        let (status, reply) = owner_request(
            &receiver,
            Method::POST,
            &format!("/chat-api/conversations/{}/messages", sender.name()),
            serde_json::json!({"text": "reply from receiver"}),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{reply}");
        wait_message(
            &receiver,
            sender.name(),
            reply["id"].as_str().unwrap(),
            "sent",
        )
        .await;
        let (status, page) = owner_request(
            sender,
            Method::GET,
            "/chat-api/conversations/receiver.dhttp.net/messages",
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            page["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| message["direction"] == "incoming"
                    && message["text"] == "reply from receiver")
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
