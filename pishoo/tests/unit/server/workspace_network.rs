//! Explicit real QUIC acceptance test. Run alone: it owns process-wide TLS/DNS/home state.
use std::{collections::BTreeMap, fmt, time::Duration};

use bytes::Bytes;
use dhttp::resolve::{EndpointAddr, Family, Resolve, ResolveFuture, Source};
use dhttp_home::DhttpHome;
use futures::{FutureExt, StreamExt};
use http::{Method, StatusCode};
use tokio::time::Instant;

use super::{
    ocsp::renew_ocsp,
    run::{apply_ocsp, load_profile, reconcile_profiles},
    *,
};
use crate::test_credentials as credentials;

#[path = "transport.rs"]
mod transport;

#[path = "lib_http.rs"]
mod lib_http;

#[path = "workspace_split.rs"]
mod workspace_split;

#[path = "std_workflow.rs"]
mod std_workflow;

#[tokio::test]
#[ignore = "run alone; requires OpenSSL/local TCP+UDP and owns global TLS/DNS/DHTTP_HOME"]
async fn websocket_real_h3_ha_roundtrip() {
    use futures::TryStreamExt;
    use http_body_util::BodyExt;
    use tokio_tungstenite::{WebSocketStream, tungstenite::protocol::Role};
    use tokio_util::io::StreamReader;

    use crate::routes::tests::websocket::{ha_exchange, ha_request, ha_upstream};

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
    let client = dhttp::Endpoint::load("alice").await.unwrap();
    let (upstream, task) = ha_upstream().await;
    let _upstream = scopeguard::guard(task, |task| task.abort());
    let profile = profile(root.path(), "receiver");
    let config =
        crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    for loaded_lib in [false, true] {
        if loaded_lib {
            let mut wasm =
                include_bytes!("../../fixtures/wasi-http-read-request-then-respond.wasm").to_vec();
            let manifest = br#"{"openapi":"3.1.0","info":{"title":"Note fixture","version":"1"},"paths":{"/upload":{"post":{}}}}"#;
            let mut section = vec![14];
            section.extend_from_slice(b"pishoo:openapi");
            section.extend_from_slice(manifest);
            wasm.push(0);
            let mut length = section.len();
            loop {
                let byte = (length & 127) as u8;
                length >>= 7;
                wasm.push(byte | if length > 0 { 128 } else { 0 });
                if length == 0 {
                    break;
                }
            }
            wasm.extend(section);
            std::fs::create_dir_all(profile.join("lib/note")).unwrap();
            std::fs::write(profile.join("lib/note/lib/component.wasm"), wasm).unwrap();
            for location in [
                "/api/note",
                "/api/note/",
                "= /api/note",
                "= /api/note/upload",
                "/api/note/child/",
            ] {
                config.execute("DELETE FROM proxy_location", []).unwrap();
                config
                    .execute(
                        "INSERT INTO proxy_location VALUES(?1,?2)",
                        [location, &format!("http://{upstream}")],
                    )
                    .unwrap();
                let error = match Server::load(profile.clone(), runtime.clone()).await {
                    Ok(_) => panic!("conflicting proxy accepted: {location}"),
                    Err(error) => error,
                };
                assert!(matches!(error, Error::InvalidConfig(_)));
                let message = error.to_string();
                assert!(
                    message.contains(location) && message.contains("reserved"),
                    "{message}"
                );
            }
        }
        config.execute("DELETE FROM proxy_location", []).unwrap();
        // HA owns its ordinary /api paths, independently of loaded /std/api Libs.
        let location = if loaded_lib { "/api" } else { "/" };
        config
            .execute(
                "INSERT INTO proxy_location VALUES(?1,?2)",
                [location, &format!("http://{upstream}")],
            )
            .unwrap();
        if loaded_lib {
            config
                .execute(
                    "INSERT INTO proxy_location VALUES('/api/noteworthy',?1)",
                    [format!("http://{upstream}")],
                )
                .unwrap();
        }
        let mut server = Server::load(profile.clone(), runtime.clone())
            .await
            .unwrap();
        assert_eq!(
            server.sandbox.read().unwrap().libs.contains_key("note"),
            loaded_lib
        );
        server
            .access
            .set_policy(
                access_control::Method::Unspecified,
                "/api/websocket",
                access_control::Effect::Deny,
                access_control::Grantee::Named,
            )
            .await
            .unwrap();
        // This is exactly the Router constructed by production Server.load,
        // including Sandbox, proxy matching and the daccess authorization layer.
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
        let listener_task = scopeguard::guard(tokio::spawn(listener), |task| task.abort());
        let mut request = ha_request(
            "https://receiver.dhttp.net/api/websocket?case=ha",
            dhttp::WndBuf::new(64 * 1024),
        );
        request.headers_mut().remove(http::header::CONNECTION);
        request.headers_mut().remove("x-drop");
        let (writer, response) = client.from_request(request).await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), response)
                .await
                .unwrap()
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        drop(writer);
        server
            .access
            .set_policy(
                access_control::Method::Unspecified,
                "/api/websocket",
                access_control::Effect::Allow,
                access_control::Grantee::Named,
            )
            .await
            .unwrap();
        for path in ["/api/states", "/api/note"] {
            server
                .access
                .set_policy(
                    access_control::Method::Unspecified,
                    path,
                    access_control::Effect::Allow,
                    access_control::Grantee::Named,
                )
                .await
                .unwrap();
        }
        let response = client
            .get("https://receiver.dhttp.net/api/states".parse().unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "[]"
        );
        if loaded_lib {
            for (path, expected) in [
                ("/api/note", StatusCode::NOT_FOUND),
                ("/api/note/", StatusCode::NOT_FOUND),
                ("/api/note/missing", StatusCode::NOT_FOUND),
                ("/api/note/upload", StatusCode::METHOD_NOT_ALLOWED),
            ] {
                let response = client
                    .get(format!("https://receiver.dhttp.net{path}").parse().unwrap())
                    .await
                    .unwrap();
                assert_eq!(response.status(), expected, "{path}");
            }
        }
        for _ in 0..2 {
            let mut request = ha_request(
                "https://receiver.dhttp.net/api/websocket?case=ha",
                dhttp::WndBuf::new(64 * 1024),
            );
            request.headers_mut().remove(http::header::CONNECTION);
            request.headers_mut().remove("x-drop");
            let (writer, response) = client.from_request(request).await.unwrap();
            let response = tokio::time::timeout(Duration::from_secs(5), response)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.version(), http::Version::HTTP_3);
            assert_eq!(response.headers()["sec-websocket-protocol"], "ha");
            let reader = StreamReader::new(
                response
                    .into_body()
                    .into_data_stream()
                    .map_err(std::io::Error::other),
            );
            let mut ws = WebSocketStream::from_raw_socket(
                tokio::io::join(reader, writer),
                Role::Client,
                None,
            )
            .await;
            ha_exchange(&mut ws).await;
        }
        let task = scopeguard::ScopeGuard::into_inner(listener_task);
        task.abort();
        let _ = task.await;
        server.close().await.unwrap();
    }
}

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
            let peer = matches!(
                name,
                "receiver.dhttp.net"
                    | "alice.dhttp.net"
                    | "bob.dhttp.net"
                    | "usopp.li.dhttp.net"
                    | "late.dhttp.net"
                    | "broken.dhttp.net"
            );
            Ok(futures::stream::iter(peer.then_some((Source::System, self.0))).boxed())
        }
        .boxed()
    }
}

fn profile(root: &std::path::Path, name: &str) -> IdentityProfile {
    let profile = IdentityProfile::try_from(root.join(name)).unwrap();
    std::fs::create_dir_all(profile.join(".volume/sys/setting")).unwrap();
    crate::configure(&profile, None, true).unwrap();
    profile
}

async fn owner_request(
    server: &Server,
    method: Method,
    path: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let bytes = if method == Method::GET || method == Method::DELETE {
        Vec::new()
    } else {
        serde_json::to_vec(&body).unwrap()
    };
    owner_bytes(server, method, path, &bytes, "application/json").await
}

async fn owner_bytes(
    server: &Server,
    method: Method,
    path: &str,
    bytes: &[u8],
    content_type: &'static str,
) -> (StatusCode, serde_json::Value) {
    let request = Request::builder()
        .method(method)
        .uri(format!("https://{}{path}", server.name()))
        .header(http::header::CONTENT_TYPE, content_type)
        .body(dhttp::WndBuf::new(64 * 1024))
        .unwrap();
    let (mut writer, response) = server.endpoint.from_request(request).await.unwrap();
    use tokio::io::AsyncWriteExt;
    writer.write_all(bytes).await.unwrap();
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
    sign_ocsp("receiver/ssl/stapling.ocsp");
    sign_ocsp("renewed.ocsp");
    let renewed = std::fs::read(root.path().join("renewed.ocsp")).unwrap();
    let before = std::fs::read(root.path().join("receiver/ssl/stapling.ocsp")).unwrap();
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
    crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
        .unwrap()
        .execute("UPDATE settings SET listen='all'", [])
        .unwrap();
    let mut server = Server::load(profile, Arc::new(WasmRuntime::new().unwrap()))
        .await
        .unwrap();
    // Keep the in-memory configuration and application resources through the update.
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
        "/std/sys/listen",
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // A disk config change must stay unapplied; no identity/Lib reload occurs.
    rusqlite::Connection::open(server.profile.join(".volume/sys/setting/database"))
        .unwrap()
        .execute("UPDATE settings SET listen='internal'", [])
        .unwrap();

    let endpoint = renew_ocsp(&server.profile, &server.endpoint).await.unwrap();
    apply_ocsp(&mut server, endpoint).await.unwrap();
    assert_eq!(server.endpoint.local_authority().unwrap().ocsp(), renewed);
    assert_eq!(std::fs::read(server.profile.ocsp_path()).unwrap(), renewed);
    assert!(Arc::ptr_eq(&router, &server.router));
    assert_eq!(server.config.listen, "all");
    assert!(!Arc::ptr_eq(&publisher, server.publisher.as_ref().unwrap()));
    // The existing pooled connection remains usable after renewal.
    let (status, settings) = owner_request(
        &server,
        Method::GET,
        "/std/sys/listen",
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
    // TLS succeeds with the renewed staple; native ACL still denies anonymous access.
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
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
    std::fs::write(root.path().join("expired/ssl/stapling.ocsp"), [1]).unwrap();
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
    // Browser credentials alone must not enroll an identity in Pishoo, at startup
    // or on subsequent discovery scans. Use valid credentials to catch initialization.
    let browser = IdentityProfile::try_from(root.path().join("valid")).unwrap();
    assert!(!browser.join(".volume/sys/setting/database").exists());
    assert!(
        load_profile(browser.clone(), runtime.clone())
            .await
            .unwrap()
            .is_none()
    );
    let mut servers = BTreeMap::new();
    assert!(
        !reconcile_profiles(
            &ddns::mdns::MdnsResolverSet::new("pishoo-test.local"),
            &DhttpHome::new(root.path().to_path_buf()),
            None,
            &mut servers,
            runtime.clone()
        )
        .await
        .unwrap()
    );
    assert!(servers.is_empty());
    assert!(!browser.join(".volume/sys/setting").exists());
    assert!(!browser.join("file").exists());
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
            !identity.join(".volume/sys/setting").exists(),
            "{name} must not be initialized"
        );
    }
    let mut valid = load_profile(profile(root.path(), "valid"), runtime.clone())
        .await
        .unwrap()
        .expect("a valid identity after invalid profiles must load");
    assert_eq!(valid.name(), "valid.dhttp.net");
    valid.close().await.unwrap();
    // A real empty v1 bootstrap is still supported through the run loop's loader.
    let empty_profile = IdentityProfile::try_from(root.path().join("valid")).unwrap();
    std::fs::write(empty_profile.join(".volume/sys/setting/database"), []).unwrap();
    let mut recovered = load_profile(empty_profile, runtime.clone())
        .await
        .unwrap()
        .unwrap();
    assert_ne!(recovered.config.listen, "off");
    assert_eq!(recovered.config.listen, "all");
    recovered.close().await.unwrap();

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
    crate::setup::open_config(&config, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
        .unwrap()
        .execute_batch("UPDATE settings SET listen=99;")
        .unwrap();
    assert!(matches!(
        load_profile(config, runtime.clone()).await,
        Err(Error::InvalidConfig(_))
    ));
    let lib = profile(root.path(), "bad-lib");
    load_server_config(&lib).unwrap();
    crate::setup::open_config(&lib, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
        .unwrap()
        .execute(
            "INSERT INTO package(has_lib,has_app,name,author,version) VALUES(1,0,'broken',NULL,'1.0.0')",
            [],
        )
        .unwrap();
    std::fs::create_dir_all(lib.join("lib/broken")).unwrap();
    std::fs::write(lib.join("lib/broken/.unused-payload"), b"not WASM").unwrap();
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

    // Exercise the new disk APIs through real TLS, daccess and H3 transport.
    let mut wasm =
        include_bytes!("../../fixtures/wasi-http-stream-response-until-cancelled.wasm").to_vec();
    let doc = br#"{"openapi":"3.1.0","info":{"title":"Test","version":"1"},"paths":{"/upload":{"post":{}}}}"#;
    let mut section = vec![14];
    section.extend_from_slice(b"pishoo:openapi");
    section.extend_from_slice(doc);
    wasm.push(0);
    let mut length = section.len();
    loop {
        let b = (length & 127) as u8;
        length >>= 7;
        wasm.push(b | if length > 0 { 128 } else { 0 });
        if length == 0 {
            break;
        }
    }
    wasm.extend(section);
    let (status, saved) = owner_bytes(
        &server,
        Method::PUT,
        "/std/sys/pkg?name=note",
        &wasm,
        "application/wasm",
    )
    .await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{saved}");
    // Seed an existing registered resource through the internal store-storage
    // helper; public HTTP writes no longer accept a local package body.
    let runtime = server.sandbox.read().unwrap().runtime.clone();
    crate::package::install_lib(
        &server.profile,
        "note",
        &crate::package::tests::package_component(&wasm),
        &runtime,
    )
    .unwrap();
    assert!(!server.sandbox.read().unwrap().libs.contains_key("note"));
    assert_eq!(
        owner_request(
            &server,
            Method::GET,
            "/std/sys/pkg/list",
            serde_json::Value::Null
        )
        .await
        .1[0]["id"],
        "note"
    );
    assert!(server.sandbox.read().unwrap().libs.is_empty());
    for (location, upstream) in [
        ("/one", "127.0.0.1:8080"),
        ("/two", "http://127.0.0.1:8081/"),
    ] {
        assert_eq!(
            owner_request(
                &server,
                Method::PATCH,
                "/std/sys/forward",
                serde_json::json!({"location":location,"proxy_pass":upstream})
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    assert_eq!(
        owner_request(
            &server,
            Method::GET,
            "/std/sys/forward?location=%2Fone",
            serde_json::Value::Null
        )
        .await
        .1["location"],
        "/one"
    );
    assert_eq!(
        owner_request(
            &server,
            Method::DELETE,
            "/std/sys/forward?location=%2Fone",
            serde_json::Value::Null
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );

    let (status, settings) = owner_request(
        &server,
        Method::GET,
        "/std/sys/listen",
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{settings}");
    assert_eq!(settings, serde_json::json!({"listen":"off"}));
    let (status, settings) = owner_request(
        &server,
        Method::PATCH,
        "/std/sys/listen",
        serde_json::json!({"listen":"internal"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{settings}");
    assert_eq!(settings, serde_json::json!({"listen":"internal"}));
    assert_eq!(
        crate::setup::load_server_config(&server.profile)
            .unwrap()
            .listen,
        "internal"
    );
    assert_eq!(
        server.config.listen, "off",
        "settings remain desired until the run loop applies them"
    );
    let proxies =
        serde_json::json!([{"location":"/service/","proxy_pass":"http://127.0.0.1:8080/"}]);
    let (status, saved) =
        owner_request(&server, Method::PUT, "/std/sys/forward", proxies.clone()).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved, proxies);
    assert!(
        server.config.proxy_locations.is_empty(),
        "proxies remain desired until the run loop applies them"
    );
    server.close().await.unwrap();
    server = Server::load(
        server.profile.clone(),
        Arc::new(WasmRuntime::new().unwrap()),
    )
    .await
    .unwrap();
    assert_eq!(
        server.config.listen, "internal",
        "restart loads the saved listen setting"
    );
    assert_eq!(server.config.proxy_locations[0].location, "/service/");
    *app.write().unwrap() = server.router.read().unwrap().clone();
    assert!(server.sandbox.read().unwrap().libs.contains_key("note"));
    use http_body_util::BodyExt;
    use tokio::io::AsyncWriteExt;
    let (mut upload, response) = server
        .endpoint
        .post(
            format!("https://{}/std/upload", server.name())
                .parse()
                .unwrap(),
        )
        .await
        .unwrap();
    upload.shutdown().await.unwrap();
    let mut response = response.await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    assert!(
        response
            .body_mut()
            .frame()
            .await
            .unwrap()
            .unwrap()
            .is_data()
    );
    drop(response);

    let (status, saved) = owner_request(
        &server,
        Method::GET,
        "/std/sys/forward",
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved, proxies);
    let (status, _) = owner_request(
        &server,
        Method::PUT,
        "/std/sys/forward",
        serde_json::json!([{"location":"/std/sys/listen","proxy_pass":"127.0.0.1:8080"}]),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        owner_request(
            &server,
            Method::GET,
            "/std/sys/forward",
            serde_json::Value::Null
        )
        .await
        .1,
        proxies
    );

    assert_eq!(
        owner_request(
            &server,
            Method::GET,
            "/std/sys/pkg/list",
            serde_json::Value::Null
        )
        .await
        .1[0]["id"],
        "note"
    );
    assert_eq!(
        owner_request(
            &server,
            Method::DELETE,
            "/std/sys/pkg?name=note",
            serde_json::Value::Null
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert!(server.sandbox.read().unwrap().libs.is_empty());
    assert!(!server.profile.join("lib/note.wasm").exists());
    assert!(server.profile.join(".volume/note").is_dir());
    // Even an explicit daccess allow cannot make another TLS identity the profile owner.
    server
        .access
        .set_policy(
            access_control::Method::Unspecified,
            "/std/sys",
            access_control::Effect::Allow,
            access_control::Grantee::All,
        )
        .await
        .unwrap();
    let alice = dhttp::Endpoint::load("alice").await.unwrap();
    let response = alice
        .get(
            format!("https://{}/std/sys/listen", server.name())
                .parse()
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.version(), http::Version::HTTP_3);
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = alice
        .get(
            format!("https://{}/std/sys/pkg/list", server.name())
                .parse()
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let response = dhttp::Anonymous
        .get(
            format!("https://{}/std/sys/listen", server.name())
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
            "/std/sys",
            access_control::Effect::Deny,
            access_control::Grantee::One(server.name().to_owned()),
        )
        .await
        .unwrap();
    let (status, _) = owner_request(
        &server,
        Method::GET,
        "/std/sys/listen",
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    server.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "run alone; exercises the live run loop with OpenSSL/local HTTP+QUIC, process TLS/DNS/home and SIGTERM"]
async fn live_run_discovers_enables_disables_and_updates_proxies_without_restart() {
    #[cfg(target_os = "linux")]
    assert!(
        !rustix::process::geteuid().is_root(),
        "run this isolated user-home scenario as an ordinary user"
    );
    let local = tokio::task::LocalSet::new();
    let scenario = async {
        let root = tempfile::tempdir().unwrap();
        let identity_home = root.path().join(".dhttp");
        std::fs::create_dir(&identity_home).unwrap();
        credentials::generate(
            identity_home.as_path(),
            &[
                ("receiver", "receiver.dhttp.net"),
                ("late", "late.dhttp.net"),
                ("broken", "broken.dhttp.net"),
            ],
        );
        let old_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", root.path());
        }
        let _environment = scopeguard::guard(old_home, |previous| unsafe {
            match previous {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        });
        dhttp::DhttpNetwork::init().await.unwrap();
        qtls::RootCerts::set([dhttp::CertificateDer::from(
            std::fs::read(identity_home.as_path().join("ca.der")).unwrap(),
        )])
        .unwrap();
        let _ = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_env_filter("warn,pishoo=info")
            .try_init();
        let bindings = qprotocol::AddressBook::global().inner_bindings();
        let (address, _) = bindings
            .iter()
            .find(|(address, device)| {
                address.is_ipv4() && (device.name() == "en0" || device.name() == "eth0")
            })
            .or_else(|| bindings.iter().find(|(address, _)| address.is_ipv4()))
            .expect("a local internal IPv4 binding");
        dhttp::resolve::Resolver::add(Arc::new(PeerResolver(EndpointAddr::direct(*address))));
        let receiver_profile = profile(identity_home.as_path(), "receiver");
        let db = crate::setup::open_config(
            &receiver_profile,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
        )
        .unwrap();
        db.execute("UPDATE settings SET listen='internal'", [])
            .unwrap();
        for name in ["late", "broken"] {
            let identity = profile(identity_home.as_path(), name);
            crate::setup::open_config(&identity, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
                .unwrap()
                .execute("UPDATE settings SET listen='internal'", [])
                .unwrap();
            std::fs::rename(
                identity.path(),
                root.path()
                    .join(".dhttp")
                    .as_path()
                    .join(format!(".{name}")),
            )
            .unwrap();
        }
        std::fs::create_dir_all(receiver_profile.join("file")).unwrap();
        std::fs::write(receiver_profile.join("file/keep.txt"), b"kept").unwrap();
        let receiver = dhttp::Endpoint::load("receiver").await.unwrap();
        let mut running = scopeguard::guard(tokio::task::spawn_local(run()), |task| task.abort());
        wait_live_body(&receiver, "/std/file/keep.txt", b"kept").await;

        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream.local_addr().unwrap();
        let _upstream = scopeguard::guard(
            tokio::spawn(async move {
                loop {
                    let (socket, _) = upstream.accept().await.unwrap();
                    tokio::spawn(async move {
                        hyper::server::conn::http1::Builder::new()
                            .serve_connection(
                                hyper_util::rt::TokioIo::new(socket),
                                hyper::service::service_fn(
                                    |request: Request<hyper::body::Incoming>| async move {
                                        Ok::<_, std::convert::Infallible>(http::Response::new(
                                            http_body_util::Full::new(Bytes::from(format!(
                                                "live {}",
                                                request.uri()
                                            ))),
                                        ))
                                    },
                                ),
                            )
                            .await
                            .unwrap();
                    });
                }
            }),
            |task| task.abort(),
        );
        // HTTP writes return the desired settings; the run loop applies them asynchronously.
        let request = Request::builder()
            .method(Method::PATCH)
            .uri("https://receiver.dhttp.net/std/sys/forward")
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(dhttp::WndBuf::new(64 * 1024))
            .unwrap();
        let (mut writer, response) = receiver.from_request(request).await.unwrap();
        use tokio::io::AsyncWriteExt;
        writer.write_all(serde_json::to_string(&serde_json::json!({"location":"/live", "proxy_pass":format!("http://{upstream_address}")})).unwrap().as_bytes()).await.unwrap();
        writer.shutdown().await.unwrap();
        assert_eq!(response.await.unwrap().status(), StatusCode::OK);
        assert!(
            std::process::Command::new("kill")
                .args(["-HUP", &std::process::id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        wait_live_body(&receiver, "/live", b"live /live").await;
        // Scope changes unregister and re-register this same Server without worker reload.
        crate::setup::config_database(
            &receiver_profile,
            &Method::PATCH,
            &"/std/sys/listen".parse().unwrap(),
            Some(serde_json::json!({"listen":"off"})),
        )
        .unwrap();
        wait_live_rejection(&receiver, "/std/file/keep.txt").await;
        assert!(
            db.query_row("SELECT listen FROM settings", [], |row| row
                .get::<_, String>(0))
                .unwrap()
                == "off"
        );
        crate::setup::config_database(
            &receiver_profile,
            &Method::PATCH,
            &"/std/sys/listen".parse().unwrap(),
            Some(serde_json::json!({"listen":"internal"})),
        )
        .unwrap();
        wait_live_body(&receiver, "/live", b"live /live").await;
        // Same Endpoint/connection pool before and after disable; persisted scope survives.
        crate::setup::config_database(
            &receiver_profile,
            &Method::PATCH,
            &"/std/sys/listen".parse().unwrap(),
            Some(serde_json::json!({"listen":"off"})),
        )
        .unwrap();
        wait_live_rejection(&receiver, "/std/file/keep.txt").await;
        assert_eq!(
            db.query_row("SELECT listen FROM settings", [], |row| row
                .get::<_, String>(0))
                .unwrap(),
            "off"
        );
        crate::setup::config_database(
            &receiver_profile,
            &Method::PATCH,
            &"/std/sys/listen".parse().unwrap(),
            Some(serde_json::json!({"listen":"internal"})),
        )
        .unwrap();
        wait_live_body(&receiver, "/std/file/keep.txt", b"kept").await;
        wait_live_body(&receiver, "/live", b"live /live").await;

        // A broken new profile is retried without preventing a later valid one from loading.
        std::fs::create_dir_all(identity_home.as_path().join(".broken/lib/bad")).unwrap();
        std::fs::write(
            root.path()
                .join(".dhttp")
                .as_path()
                .join(".broken/lib/bad/.unused-payload"),
            b"broken",
        )
        .unwrap();
        std::fs::rename(
            identity_home.as_path().join(".broken"),
            identity_home.as_path().join("broken"),
        )
        .unwrap();
        std::fs::rename(
            identity_home.as_path().join(".late"),
            identity_home.as_path().join("late"),
        )
        .unwrap();
        let late_profile = IdentityProfile::try_from(identity_home.as_path().join("late")).unwrap();
        std::fs::create_dir_all(late_profile.join("file")).unwrap();
        std::fs::write(late_profile.join("file/late.txt"), b"discovered").unwrap();
        let late = dhttp::Endpoint::load("late").await.unwrap();
        wait_live_body(&late, "/std/file/late.txt", b"discovered").await;
        wait_live_body(&receiver, "/live", b"live /live").await;
        std::fs::rename(late_profile.path(), identity_home.as_path().join(".late")).unwrap();
        wait_live_rejection(&late, "/std/file/late.txt").await;
        wait_live_body(&receiver, "/live", b"live /live").await;

        assert!(
            std::process::Command::new("/bin/kill")
                .args(["-TERM", &std::process::id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        tokio::time::timeout(Duration::from_secs(20), &mut *running)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        // Shutdown releases the original listener registration as well.
        let listener = receiver
            .listen(
                dhttp::Scope::Internal.into(),
                tower::service_fn(|_: Request<Body>| async {
                    Ok::<_, std::convert::Infallible>(http::Response::new(http_body_util::Empty::<
                        Bytes,
                    >::new(
                    )))
                }),
            )
            .await
            .unwrap();
        drop(listener);
    };
    local.run_until(scenario).await;
}

async fn wait_live_body(endpoint: &dhttp::Endpoint, path: &str, expected: &[u8]) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let uri = format!("https://{}{path}", endpoint.name())
            .parse()
            .unwrap();
        if let Ok(Ok(response)) =
            tokio::time::timeout(Duration::from_secs(2), endpoint.get(uri)).await
            && response.status() == StatusCode::OK
            && let Ok(Ok(bytes)) = tokio::time::timeout(
                Duration::from_secs(2),
                axum::body::to_bytes(AxumBody::new(response.into_body()), 64 * 1024),
            )
            .await
            && bytes.as_ref() == expected
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{path} did not return expected body"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_live_rejection(endpoint: &dhttp::Endpoint, path: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let uri = format!("https://{}{path}", endpoint.name())
            .parse()
            .unwrap();
        if let Ok(Err(_)) = tokio::time::timeout(Duration::from_secs(2), endpoint.get(uri)).await {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{path} still accepts requests after disable/removal"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
