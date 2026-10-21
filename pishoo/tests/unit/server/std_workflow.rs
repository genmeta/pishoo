//! Pack real std sources, install over authenticated HTTP/3, then exercise crossed friendship.
//! Run alone with pishoo/tests/run-std-workflow.py; all identities and databases are temporary.
use std::path::Path;

use serde_json::{Value, json};

use super::*;

async fn remote_json(
    endpoint: &dhttp::Endpoint,
    host: &str,
    method: Method,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    use tokio::io::AsyncWriteExt;
    let request = Request::builder()
        .method(method)
        .uri(format!("https://{host}{path}"))
        .header("content-type", "application/json")
        .body(dhttp::WndBuf::new(64 * 1024))
        .unwrap();
    let (mut writer, response) = endpoint.from_request(request).await.unwrap();
    if !body.is_null() {
        writer
            .write_all(&serde_json::to_vec(&body).unwrap())
            .await
            .unwrap();
    }
    writer.shutdown().await.unwrap();
    let response = tokio::time::timeout(Duration::from_secs(15), response)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.version(), http::Version::HTTP_3);
    let status = response.status();
    let bytes = axum::body::to_bytes(AxumBody::new(response.into_body()), 1024 * 1024)
        .await
        .unwrap();
    let body = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| json!({"body":String::from_utf8_lossy(&bytes)}));
    (status, body)
}

async fn expect(server: &Server, method: Method, path: &str, body: Value, status: u16) -> Value {
    let request = async {
        if body.is_null() {
            owner_bytes(server, method, path, &[], "application/json").await
        } else {
            owner_request(server, method, path, body).await
        }
    };
    let (actual, body) = tokio::time::timeout(Duration::from_secs(20), request)
        .await
        .unwrap();
    assert_eq!(actual.as_u16(), status, "{} {path}: {body}", server.name());
    body
}

async fn rule(server: &Server, peer: &str, method: &str, path: &str, effect: &str) {
    expect(
        server,
        Method::PATCH,
        &format!("/std/acl/allow/{peer}"),
        json!({"method":method,"api":path,"effect":effect}),
        204,
    )
    .await;
}

async fn jobs(server: &Server) {
    crate::contact::contact_jobs(
        &server.profile,
        &server.endpoint,
        &server.access,
        &server.contact_db,
    )
    .await
    .unwrap();
    let native = native_router(server).layer(axum::middleware::from_fn_with_state(
        server.access.clone(),
        crate::routes::authorize,
    ));
    tokio::time::timeout(
        Duration::from_secs(20),
        Sandbox::run(
            server.sandbox.clone(),
            server.endpoint.clone(),
            server.access.clone(),
            native,
        ),
    )
    .await
    .unwrap();
}

async fn refresh(server: &Server, id: &Value) -> Value {
    expect(
        server,
        Method::POST,
        &format!("/std/contact/requests/refresh?id={id}"),
        Value::Null,
        200,
    )
    .await;
    jobs(server).await;
    expect(
        server,
        Method::GET,
        &format!("/std/contact/requests?id={id}"),
        Value::Null,
        200,
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "run pishoo/tests/run-std-workflow.py; real pack binaries, std builds, OpenSSL and QUIC; run alone"]
async fn clean_identities_pack_install_friendship_approval_order_and_chat() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home/.dhttp");
    credentials::generate(
        &home,
        &[("alice", "alice.dhttp.net"), ("bob", "bob.dhttp.net")],
    );
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let ssl = home.join("std/ssl");
    std::fs::create_dir_all(&ssl).unwrap();
    std::fs::copy(
        repo.join("std.dhttp.net.crt.pem"),
        ssl.join("fullchain.crt"),
    )
    .unwrap();
    std::fs::copy(repo.join("std.dhttp.net.key.pem"), ssl.join("privkey.pem")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            ssl.join("privkey.pem"),
            std::fs::Permissions::from_mode(0o400),
        )
        .unwrap();
    }
    let projects =
        std::env::var_os("PISHOO_STD_E2E_PROJECTS").expect("run pishoo/tests/run-std-workflow.py");
    let genmeta =
        std::env::var_os("GENMETA_PACK_TEST_BIN").expect("new gmutils genmeta binary required");
    let mut projects = std::fs::read_dir(projects)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    projects.sort(); // lib-* precede app-* only after explicit kind sorting below.
    let mut packages = Vec::new();
    for project in projects {
        let output = root.path().join(format!(
            "{}.webc",
            project.file_name().unwrap().to_str().unwrap()
        ));
        let result = std::process::Command::new(&genmeta)
            .args(["build", "--no-build"])
            .arg(&project)
            .args(["--id", "std.dhttp.net", "--output"])
            .arg(&output)
            .env("HOME", home.parent().unwrap())
            .env_remove("DHTTP_HOME")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "genmeta build {}: {}",
            project.display(),
            String::from_utf8_lossy(&result.stderr)
        );
        let metadata: Value = serde_json::from_slice(&result.stdout).unwrap();
        eprintln!(
            "PACK {} {} {}",
            metadata["kind"], metadata["id"], metadata["version"]
        );
        let bytes = std::fs::read(output).unwrap();
        let package = crate::package::read_package(&bytes).unwrap();
        let metadata = crate::package::package_metadata(&package).unwrap();
        packages.push((metadata, bytes));
    }
    packages.sort_by_key(|(metadata, _)| metadata["kind"] == "app");
    assert!(packages.iter().any(|(m, _)| m["name"] == "workspace"));
    assert!(packages.iter().any(|(m, _)| m["name"] == "message"));
    assert!(packages.iter().any(|(m, _)| m["name"] == "profile"));
    dhttp::DhttpNetwork::init().await.unwrap();
    qtls::RootCerts::set([dhttp::CertificateDer::from(
        std::fs::read(home.join("ca.der")).unwrap(),
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
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let mut servers = Vec::new();
    let mut listeners = Vec::new();
    for name in ["alice", "bob"] {
        // A fresh config deliberately suppresses distribution auto-install. Every package below uses PUT.
        let server = Server::load(profile(&home, name), runtime.clone())
            .await
            .unwrap();
        assert!(server.sandbox.read().unwrap().libs.is_empty());
        assert!(server.sandbox.read().unwrap().apps.is_empty());
        let router = server.router.clone();
        let listener = server
            .endpoint
            .listen(
                dhttp::Scope::Loopback.into(),
                tower::service_fn(move |request: Request<Body>| {
                    let router = router.read().unwrap().clone();
                    async move { router.oneshot(request.map(AxumBody::new)).await }
                }),
            )
            .await
            .unwrap();
        listeners.push(tokio::spawn(listener));
        eprintln!("CLEAN {}: zero app, zero lib", server.name());
        for (metadata, bytes) in &packages {
            let category = if metadata["kind"] == "app" {
                "app"
            } else {
                "lib"
            };
            let name = metadata["name"].as_str().unwrap();
            let path = format!(
                "/std/sys/{}?name={name}",
                if category == "app" { "app" } else { "lib" }
            );
            let (status, body) = tokio::time::timeout(
                Duration::from_secs(90),
                owner_bytes(&server, Method::PUT, &path, bytes, "application/webc"),
            )
            .await
            .expect("package installation timed out");
            assert_eq!(status, StatusCode::OK, "{path}: {body}");
            assert!(
                server
                    .profile
                    .join(format!("{category}/std/{name}/manifest.json"))
                    .exists()
            );
            eprintln!("INSTALL {} {path}: {}", server.name(), status);
        }
        let db =
            rusqlite::Connection::open(server.profile.join(".volume/sys/setting/.db")).unwrap();
        let count: i64 = db
            .query_row(
                "SELECT (SELECT count(*) FROM lib)+(SELECT count(*) FROM app)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count as usize, packages.len());
        expect(&server, Method::GET, "/std/workspace/", Value::Null, 200).await;
        if packages
            .iter()
            .any(|(m, _)| m["id"] == "std.dhttp.net/calendar" && m["kind"] == "app")
        {
            expect(&server, Method::GET, "/std/calendar/", Value::Null, 200).await;
            let events = expect(
                &server,
                Method::GET,
                "/std/calendar/list?from=1791936000000&to=1794528000000",
                Value::Null,
                200,
            )
            .await;
            assert_eq!(events["events"], json!([]));
        }
        expect(
            &server,
            Method::PATCH,
            "/std/profile",
            json!({"display_name":name}),
            200,
        )
        .await;
        // Contact discovery uses the host defaults, without test-only grants.
        servers.push(server);
    }
    let _listeners = scopeguard::guard(listeners, |tasks| {
        for task in tasks {
            task.abort();
        }
    });
    let alice = &servers[0];
    let bob = &servers[1];
    let (_, intro) = remote_json(
        &bob.endpoint,
        alice.name(),
        Method::GET,
        "/std/profile/context",
        Value::Null,
    )
    .await;
    assert_eq!(intro["viewer_name"], bob.name());
    assert_eq!(intro["is_owner"], false);
    let denied = remote_json(
        &bob.endpoint,
        alice.name(),
        Method::PUT,
        "/std/sys/pkg?name=workspace",
        json!({}),
    )
    .await;
    assert_eq!(denied.0, StatusCode::FORBIDDEN);
    let catalog = expect(alice, Method::GET, "/std/acl/permissions", Value::Null, 200).await;
    assert_eq!(
        catalog["std.dhttp.net/message · message"],
        json!({"/std/message":["POST"]})
    );
    assert!(catalog.as_object().unwrap().values().all(|paths| {
        !paths
            .as_object()
            .unwrap()
            .keys()
            .any(|path| path.starts_with("/std/acl"))
    }));

    // Each permutation starts with no friendship and denied messaging. Each action occurs via HTTP.
    // 0/1: accept on Alice/Bob; 2/3: persistent Message grant on Alice/Bob.
    let orders = [
        [0, 1, 2, 3],
        [2, 3, 0, 1],
        [2, 0, 3, 1],
        [1, 3, 0, 2],
        [3, 2, 1, 0],
        [0, 2, 1, 3],
    ];
    for (case, order) in orders.into_iter().enumerate() {
        for (server, peer) in [(alice, bob), (bob, alice)] {
            expect(
                server,
                Method::DELETE,
                &format!("/std/contact?name={}", peer.name()),
                Value::Null,
                if case == 0 { 404 } else { 204 },
            )
            .await;
            rule(server, "**", "POST", "/std/message", "deny").await;
        }
        let payload = |target: &str| json!({"target_name":target,"description":format!("crossed case {case}"),"requested_capabilities":["message"],"offered_capabilities":["message"]});
        let (a, b) = tokio::join!(
            expect(
                alice,
                Method::POST,
                "/std/contact",
                payload(bob.name()),
                201
            ),
            expect(
                bob,
                Method::POST,
                "/std/contact",
                payload(alice.name()),
                201
            )
        );
        tokio::join!(jobs(alice), jobs(bob));
        assert_eq!(
            expect(
                alice,
                Method::GET,
                &format!("/std/contact?name={}", bob.name()),
                Value::Null,
                200
            )
            .await["status"],
            "pending"
        );
        assert_eq!(
            expect(
                bob,
                Method::GET,
                &format!("/std/contact?name={}", alice.name()),
                Value::Null,
                200
            )
            .await["status"],
            "pending"
        );
        if case == 0 {
            // Approving the actual request persists ACL independently of contact acceptance.
            rule(bob, alice.name(), "POST", "/std/message", "review").await;
            let payload = json!({"client_message_id":"review-before-contact","content":"先审批后加好友","media_type":"text/plain"});
            let (status, pending) = remote_json(
                &alice.endpoint,
                bob.name(),
                Method::POST,
                "/std/message?scenario=before-contact",
                payload.clone(),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{pending}");
            expect(
                bob,
                Method::PATCH,
                "/std/acl/review",
                json!({"id":pending["review_id"],"action":"allow"}),
                204,
            )
            .await;
            let (status, body) = remote_json(
                &alice.endpoint,
                bob.name(),
                Method::POST,
                "/std/message?scenario=before-contact",
                payload,
            )
            .await;
            assert_eq!(status, StatusCode::CREATED, "approved request: {body}");
            rule(bob, alice.name(), "POST", "/std/message", "deny").await;
            eprintln!("PASS request approval is independent of contact acceptance");
        }
        let mut granted = [false, false];
        for action in order {
            let (server, peer) = if action % 2 == 0 {
                (alice, bob)
            } else {
                (bob, alice)
            };
            if action < 2 {
                let path = format!("/std/contact?name={}", peer.name());
                let before = expect(server, Method::GET, &path, Value::Null, 200).await;
                expect(
                    server,
                    Method::PATCH,
                    &path,
                    json!({"status":"active"}),
                    if before["status"] == "active" {
                        409
                    } else {
                        204
                    },
                )
                .await;
                let after = expect(server, Method::GET, &path, Value::Null, 200).await;
                assert_eq!(after["id"], before["id"]);
                assert_eq!(after["subject_id"], before["subject_id"]);
                assert_eq!(after["status"], "active");
            } else {
                rule(server, peer.name(), "POST", "/std/message", "allow").await;
                granted[action % 2] = true;
            }
            let a = refresh(alice, &a["id"]).await;
            let b = refresh(bob, &b["id"]).await;
            eprintln!(
                "ORDER case={case} action={action} alice={} bob={}",
                a["status"], b["status"]
            );
            for (receiver, sender) in [(alice, bob), (bob, alice)] {
                let contact = expect(
                    receiver,
                    Method::GET,
                    &format!("/std/contact?name={}", sender.name()),
                    Value::Null,
                    200,
                )
                .await;
                let allowed = granted[usize::from(receiver.name() == bob.name())];
                let (status, body) = remote_json(&sender.endpoint, receiver.name(), Method::POST, "/std/message", json!({"client_message_id":format!("order-{case}-{action}-{}",sender.name().trim_end_matches(".dhttp.net")),"content":"乱序权限探测","media_type":"text/plain"})).await;
                assert_eq!(
                    status.as_u16(),
                    if allowed { 201 } else { 403 },
                    "case {case} action {action}: {contact}; response {body}"
                );
            }
        }
        for (server, peer, request) in [(alice, bob, &a), (bob, alice, &b)] {
            assert_eq!(refresh(server, &request["id"]).await["status"], "active");
            let text = format!("第{case}轮 {} → {}，你好 👋", server.name(), peer.name());
            let sent = expect(
                server,
                Method::PUT,
                "/std/message",
                json!({"to":peer.name(),"content":text,"media_type":"text/plain","client_message_id":format!("queued-{case}-{}",server.name())}),
                202,
            )
            .await;
            jobs(server).await;
            let local = expect(
                server,
                Method::GET,
                &format!("/std/message/list?name={}", peer.name()),
                Value::Null,
                200,
            )
            .await;
            assert!(
                local["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|m| m["index"] == sent["index"] && m["state"] == "sent"),
                "{local}"
            );
            let received = expect(
                peer,
                Method::GET,
                &format!("/std/message/list?name={}", server.name()),
                Value::Null,
                200,
            )
            .await;
            assert!(
                received["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|m| m["content"] == text && m["direction"] == "incoming"),
                "{received}"
            );
            expect(
                server,
                Method::DELETE,
                &format!("/std/contact/requests?id={}", request["id"]),
                Value::Null,
                204,
            )
            .await;
        }
        eprintln!(
            "PASS case={case} order={order:?}: crossed requests active, bidirectional messages delivered"
        );
    }

    // Review permission and friendship acceptance are independent; approval persists an ordinary ACL grant.
    rule(bob, alice.name(), "POST", "/std/message", "review").await;
    let payload = json!({"client_message_id":"review-order","content":"请求审批重试","media_type":"text/plain"});
    let (status, pending) = remote_json(
        &alice.endpoint,
        bob.name(),
        Method::POST,
        "/std/message?scenario=active-review",
        payload.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{pending}");
    let (_, repeated) = remote_json(
        &alice.endpoint,
        bob.name(),
        Method::POST,
        "/std/message?scenario=active-review",
        payload.clone(),
    )
    .await;
    assert_eq!(pending["review_id"], repeated["review_id"]);
    expect(
        bob,
        Method::PATCH,
        "/std/acl/review",
        json!({"id":pending["review_id"],"action":"allow"}),
        204,
    )
    .await;
    let (_, state) = remote_json(
        &alice.endpoint,
        bob.name(),
        Method::GET,
        pending["status_url"].as_str().unwrap(),
        Value::Null,
    )
    .await;
    assert_eq!(state["status"], "allowed");
    let (status, receipt) = remote_json(
        &alice.endpoint,
        bob.name(),
        Method::POST,
        "/std/message?scenario=active-review",
        payload.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{receipt}");
    let (status, duplicate) = remote_json(
        &alice.endpoint,
        bob.name(),
        Method::POST,
        "/std/message?scenario=active-review",
        payload,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(duplicate["index"], receipt["index"]);
    let rules = expect(bob, Method::GET, "/std/acl/allow", Value::Null, 200).await;
    assert!(
        rules[alice.name()]["/std/message"]["allow"]
            .as_array()
            .is_some_and(|m| m.contains(&json!("POST")))
    );
    eprintln!(
        "PASS request review: deduplicated pending request, approved retry, exactly-once receive; durable ACL grant"
    );
    for server in &mut servers {
        let db =
            rusqlite::Connection::open(server.profile.join(".volume/message/message.db")).unwrap();
        assert_eq!(db.query_row("SELECT count(*) FROM sqlite_master WHERE name IN ('capability_decisions','capability_decision_events')", [], |r|r.get::<_,i64>(0)).unwrap(), 0);
        let messages = db
            .query_row("SELECT count(*) FROM chat_messages", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap();
        assert!(messages >= 12);
        drop(db);
        server.close().await.unwrap();
        let mut reloaded = Server::load(server.profile.clone(), runtime.clone())
            .await
            .unwrap();
        assert_eq!(
            reloaded.sandbox.read().unwrap().libs.len()
                + reloaded.sandbox.read().unwrap().apps.len(),
            packages.len()
        );
        let peer = if reloaded.name() == "alice.dhttp.net" {
            "bob.dhttp.net"
        } else {
            "alice.dhttp.net"
        };
        assert_eq!(
            reloaded
                .access
                .find_contact_by_name(peer)
                .await
                .unwrap()
                .status,
            access_control::ContactStatus::Active as i32
        );
        let db = rusqlite::Connection::open(reloaded.profile.join(".volume/message/message.db"))
            .unwrap();
        assert_eq!(
            db.query_row("SELECT count(*) FROM chat_messages", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            messages
        );
        reloaded.close().await.unwrap();
    }
    eprintln!("PASS reload: signed expanded artifacts, contacts and private messages preserved");
}

/// Explicit local acceptance entrypoint; regular test runs never modify a persistent profile.
#[test]
#[ignore = "requires explicit signed local artifacts and an isolated profile under target; stop its host first"]
fn install_signed_workspace_in_isolated_manual_test_profile() {
    let profile_path = std::path::PathBuf::from(
        std::env::var_os("PISHOO_MANUAL_TEST_PROFILE").expect("set PISHOO_MANUAL_TEST_PROFILE"),
    )
    .canonicalize()
    .unwrap();
    let target = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("target")
        .canonicalize()
        .unwrap();
    assert!(
        profile_path.starts_with(target),
        "manual installation is restricted to the repository test target directory"
    );
    let artifacts = std::path::PathBuf::from(
        std::env::var_os("PISHOO_MANUAL_TEST_ARTIFACTS").expect("set PISHOO_MANUAL_TEST_ARTIFACTS"),
    );
    let profile = IdentityProfile::try_from(profile_path).unwrap();
    let db =
        crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    let has_apps: bool = db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='app')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    if has_apps {
        assert_eq!(
            db.query_row("SELECT count(*) FROM app", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            0,
            "manual acceptance requires a profile with no installed App"
        );
    }
    let has_libs: bool = db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='lib')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let has_profile = has_libs
        && db
            .query_row("SELECT count(*) FROM lib WHERE name='profile'", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap()
            == 1;
    drop(db);
    let runtime = WasmRuntime::new().unwrap();
    if !has_profile {
        let bytes = std::fs::read(artifacts.join("profile-0.1.0.webc")).unwrap();
        let installed =
            crate::package::install_lib(&profile, "std.dhttp.net/profile", &bytes, &runtime)
                .unwrap();
        assert_eq!(installed["version"], "0.1.0");
        eprintln!(
            "LOCAL LIB INSTALL {} {}",
            installed["id"], installed["version"]
        );
    }
    let message = std::fs::read(artifacts.join("message-0.1.1.webc")).unwrap();
    let installed =
        crate::package::install_lib(&profile, "std.dhttp.net/message", &message, &runtime).unwrap();
    assert_eq!(installed["version"], "0.1.1");
    eprintln!(
        "LOCAL LIB INSTALL {} {}",
        installed["id"], installed["version"]
    );
    let workspace = std::fs::read(artifacts.join("workspace-0.1.4.webc")).unwrap();
    let installed =
        crate::package::install_app(&profile, "std.dhttp.net/workspace", &workspace).unwrap();
    assert_eq!(installed["version"], "0.1.4");
    eprintln!(
        "LOCAL APP INSTALL {} {}",
        installed["id"], installed["version"]
    );
    assert!(profile.join("lib/std/message.wasm").is_file());
    assert!(profile.join("app/workspace/index.html").is_file());
    let db =
        crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM app_dependency WHERE app_name='workspace'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        2
    );
}
