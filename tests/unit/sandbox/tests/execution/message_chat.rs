//! Opt-in acceptance against the independent, freshly built Message component.
use serde_json::{Value, json};

use super::*;

async fn event(body: &mut Body, pending: &mut Vec<u8>) -> String {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut checked = 0;
        loop {
            if let Some(end) = pending[checked..]
                .windows(2)
                .position(|bytes| bytes == b"\n\n")
                .map(|offset| checked + offset)
            {
                let result = String::from_utf8(pending[..end].to_vec()).unwrap();
                pending.drain(..end + 2);
                return result;
            }
            checked = pending.len().saturating_sub(1);
            let frame = body.frame().await.expect("SSE closed").unwrap();
            if let Ok(bytes) = frame.into_data() {
                pending.extend_from_slice(&bytes);
            }
        }
    })
    .await
    .expect("SSE did not observe the other WASM instance")
}

#[tokio::test]
#[ignore = "run alone with PISHOO_MESSAGE_COMPONENT; generates isolated TLS credentials"]
async fn message_chat_two_real_wasm_instances_observe_commits_without_events_wit() {
    verify_message_stream(false, false).await;
}

#[tokio::test]
#[ignore = "run alone with freshly built Message; validates 512 KiB images under the real WASI budget"]
async fn message_chat_inline_image_roundtrip_and_replay() {
    use base64::Engine;
    let bytes = std::fs::read(std::env::var_os("PISHOO_MESSAGE_COMPONENT").unwrap()).unwrap();
    let root = tempfile::tempdir().unwrap();
    let (local, owner, bob) = journal::journal_credentials(root.path()).await;
    let directory = root.path().join("message-data");
    let lib = load(&bytes, &directory);
    let tasks = TaskTracker::new();
    let invoke = |remote| {
        Invocation::new(
            lib.clone(),
            crate::test_support::identity::endpoint("alice.dhttp.net"),
            &dhttp::HandshakeSummary {
                alpn: None,
                local: Some(local.clone()),
                remote: Some(remote),
            },
            tasks.clone(),
            Outgoing::Denied,
        )
        .unwrap()
    };
    let mut image = vec![0; 512 * 1024];
    image[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
    let content = format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(image)
    );
    let request = Request::post("https://alice.dhttp.net/comm/message").header("content-type", "application/json").body(
        Full::new(Bytes::from(json!({"type":"image","from":"bob.dhttp.net","to":"alice.dhttp.net","content":content,"media_type":"image/png","client_message_id":"image-2m"}).to_string()))
            .map_err(|never| match never {}).boxed_unsync(),
    ).unwrap();
    let response = invoke(bob).execute(request).await.unwrap();
    assert_eq!(response.status(), http::StatusCode::CREATED);
    let receipt: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(receipt["content"], content);
    let response = invoke(owner.clone())
        .execute(
            Request::get("https://alice.dhttp.net/comm/message/list")
                .body(empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), http::StatusCode::OK);
    let history: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(history["items"][0]["content"], content);
    let db = rusqlite::Connection::open_with_flags_and_vfs(
        directory.join("database"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        "unix-dotfile",
    )
    .unwrap();
    let epoch: String = db
        .query_row("SELECT epoch FROM chat_stream", [], |row| row.get(0))
        .unwrap();
    drop(db);
    let request = Request::get(format!(
        "https://alice.dhttp.net/comm/message/list?cursor={epoch}:0"
    ))
    .header("accept", "text/event-stream")
    .body(empty())
    .unwrap();
    let response = invoke(owner).execute(request).await.unwrap();
    assert_eq!(response.status(), http::StatusCode::OK);
    let mut stream = response.into_body();
    let mut pending = Vec::new();
    assert!(
        event(&mut stream, &mut pending)
            .await
            .starts_with("event: ready")
    );
    let frame = event(&mut stream, &mut pending).await;
    let replay: Value = serde_json::from_str(frame.split_once("data: ").unwrap().1.trim()).unwrap();
    assert_eq!(replay["items"][0]["content"], content);
    drop(stream);
    reaped(&tasks).await;
    assert!(!directory.join("database.lock").exists());
}

#[tokio::test]
#[ignore = "run alone with freshly built Message; maximum-length backlog acceptance"]
async fn message_large_replay_budget_diagnostic() {
    verify_message_stream(true, false).await;
}

#[tokio::test]
#[ignore = "run alone with freshly built Message; escaped Unicode backlog acceptance"]
async fn message_large_escaped_replay_budget() {
    verify_message_stream(true, true).await;
}

async fn verify_message_stream(large_replay: bool, escaped: bool) {
    let bytes = std::fs::read(
        std::env::var_os("PISHOO_MESSAGE_COMPONENT")
            .expect("build Message and set PISHOO_MESSAGE_COMPONENT"),
    )
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    let (local, owner, bob) = journal::journal_credentials(root.path()).await;
    let directory = root.path().join("message-data");
    let lib = load(&bytes, &directory);
    let tasks = TaskTracker::new();
    let invoke = |remote| {
        Invocation::new(
            lib.clone(),
            crate::test_support::identity::endpoint("alice.dhttp.net"),
            &dhttp::HandshakeSummary {
                alpn: None,
                local: Some(local.clone()),
                remote: Some(remote),
            },
            tasks.clone(),
            Outgoing::Denied,
        )
        .unwrap()
    };
    let mut request = Request::get("https://alice.dhttp.net/comm/message/list")
        .body(empty())
        .unwrap();
    request
        .headers_mut()
        .insert("accept", "text/event-stream".parse().unwrap());
    let response = invoke(owner.clone()).execute(request).await.unwrap();
    assert_eq!(response.status(), http::StatusCode::OK);
    let mut stream = response.into_body();
    let mut pending = Vec::new();
    let ready = event(&mut stream, &mut pending).await;
    assert!(ready.starts_with("event: ready\n"));
    let send = |text: &str| {
        Request::post("https://alice.dhttp.net/comm/message").header("content-type", "application/json").body(Full::new(Bytes::from(json!({"type":"text","from":"bob.dhttp.net","to":"alice.dhttp.net","content":text,"client_message_id":"wasm-commit"}).to_string())).map_err(|never| match never {}).boxed_unsync()).unwrap()
    };
    let started = std::time::Instant::now();
    let response = invoke(bob.clone())
        .execute(send("跨实例消息"))
        .await
        .unwrap();
    assert_eq!(response.status(), http::StatusCode::CREATED);
    let receipt: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let message = event(&mut stream, &mut pending).await;
    assert!(message.contains("跨实例消息"));
    let cursor = message
        .lines()
        .find_map(|line| line.strip_prefix("id: "))
        .unwrap()
        .to_owned();
    println!(
        "MESSAGE_WASM_COMMIT_TO_EVENT_MS={}",
        started.elapsed().as_millis()
    );
    // A failed duplicate rolls back and leaves the subscription healthy.
    let response = invoke(bob)
        .execute(send("conflicting content"))
        .await
        .unwrap();
    assert_eq!(response.status(), http::StatusCode::CONFLICT);
    response.into_body().collect().await.unwrap();
    assert_eq!(receipt["client_message_id"], "wasm-commit");
    drop(stream);
    if large_replay {
        let db = rusqlite::Connection::open_with_flags_and_vfs(
            directory.join("database"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
            "unix-dotfile",
        )
        .unwrap();
        db.busy_timeout(Duration::from_secs(2)).unwrap();
        let tx = db.unchecked_transaction().unwrap();
        let content = if escaped {
            "你\n\"\\😀".repeat(800)
        } else {
            "你".repeat(4000)
        };
        for id in 2..=103 {
            tx.execute("INSERT INTO chat_messages(id,contact_name,client_message_id,direction,state,sender_name,recipient_name,text,created_at,updated_at) VALUES(?1,'bob.dhttp.net',?1,'incoming','received','bob.dhttp.net','alice.dhttp.net',?2,1,1)",rusqlite::params![id,content]).unwrap();
        }
        tx.commit().unwrap();
    }
    // Reconnect honors Last-Event-ID ahead of an invalid query cursor.
    let mut request = Request::get("https://alice.dhttp.net/comm/message/list?cursor=invalid")
        .body(empty())
        .unwrap();
    request
        .headers_mut()
        .insert("accept", "text/event-stream".parse().unwrap());
    request
        .headers_mut()
        .insert("last-event-id", cursor.parse().unwrap());
    let response = invoke(owner).execute(request).await.unwrap();
    assert_eq!(response.status(), http::StatusCode::OK);
    let mut stream = response.into_body();
    let ready = event(&mut stream, &mut Vec::new()).await;
    assert!(ready.contains(&format!("id: {cursor}")));
    if large_replay {
        let mut seen = std::collections::BTreeSet::new();
        let mut pending = Vec::new();
        while seen.len() < 102 {
            let frame = event(&mut stream, &mut pending).await;
            assert!(frame.len() + 2 <= 512 * 1024);
            let data: Value = serde_json::from_str(
                frame
                    .lines()
                    .find_map(|line| line.strip_prefix("data: "))
                    .unwrap(),
            )
            .unwrap();
            let items = data["items"].as_array().unwrap();
            assert!(items.len() <= 100);
            for item in items {
                assert!(seen.insert(item["index"].as_i64().unwrap()));
                assert_eq!(
                    item["content"],
                    if escaped {
                        "你\n\"\\😀".repeat(800)
                    } else {
                        "你".repeat(4000)
                    }
                );
            }
        }
    }
    drop(stream);
    reaped(&tasks).await;
    assert!(!directory.join("database.lock").exists());
}

#[tokio::test]
#[ignore = "run alone with Message component/repo; owns global DHTTP/TLS and isolated UDP"]
async fn message_chat_real_wasm_http_poll_isolates_slow_contact_and_refills_slots() {
    verify_message_delivery(false, false).await;
}

#[tokio::test]
#[ignore = "run alone with freshly built Message; legacy inline image delivery through the subscription"]
async fn message_inline_image_subscription_delivery() {
    verify_message_delivery(true, false).await;
}

#[tokio::test]
#[ignore = "run alone with freshly built Message; disconnect cancels pending delivery and reconnect resumes"]
async fn message_chat_subscription_drop_returns_inflight_lease_and_resumes() {
    verify_message_delivery(false, true).await;
}

async fn verify_message_delivery(large_image: bool, cancel_early: bool) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("pishoo=warn")
        .try_init();
    use dhttp::resolve::{EndpointAddr, Family, Resolve, ResolveFuture, Source};
    use futures::{FutureExt, StreamExt};
    #[derive(Debug)]
    struct Peer(EndpointAddr);
    impl std::fmt::Display for Peer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("Message acceptance peer")
        }
    }
    impl Resolve for Peer {
        fn lookup<'a>(&'a self, _: &'a str, _: &'a str, _: Option<Family>) -> ResolveFuture<'a> {
            async move { Ok(futures::stream::iter([(Source::System, self.0)]).boxed()) }.boxed()
        }
    }
    let root = tempfile::tempdir().unwrap();
    crate::test_support::credentials::generate(
        root.path(),
        &[
            ("alice", "alice.dhttp.net"),
            ("slow", "slow.dhttp.net"),
            ("fast", "fast.dhttp.net"),
        ],
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
    let endpoint = dhttp::Endpoint::load("alice").await.unwrap();
    let slow = dhttp::Endpoint::load("slow").await.unwrap();
    let fast = dhttp::Endpoint::load("fast").await.unwrap();
    let hash = |endpoint: &dhttp::Endpoint| {
        dhttp_home::certificate::extract_dhttp_subject_key_identifier(
            endpoint.local_authority().unwrap().certificates(),
        )
        .unwrap()
        .owner_hash()
        .as_str()
        .to_owned()
    };
    let local_hash = hash(&endpoint);
    let peers = Arc::new(std::collections::HashMap::from([
        ("slow.dhttp.net".to_owned(), hash(&slow)),
        ("fast.dhttp.net".to_owned(), hash(&fast)),
    ]));
    let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
    let slow_listener = slow
        .listen(
            dhttp::Scope::Loopback.into(),
            tower::service_fn({
                let sent = sent.clone();
                move |request: Request<Body>| {
                    let sent = sent.clone();
                    async move {
                        let mut body: Value = serde_json::from_slice(
                            &request.into_body().collect().await.unwrap().to_bytes(),
                        )
                        .unwrap();
                        sent.send("slow-started").unwrap();
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        if body["type"] == "text" && body.get("media_type").is_none() {
                            body["media_type"] = "text/plain".into();
                        }
                        body["index"] = json!(1);
                        sent.send("slow-finished").unwrap();
                        Ok::<_, std::convert::Infallible>(http::Response::new(
                            axum::body::Body::from(body.to_string()),
                        ))
                    }
                }
            }),
        )
        .await
        .unwrap();
    let fast_listener = fast
        .listen(
            dhttp::Scope::Loopback.into(),
            tower::service_fn({
                let sent = sent.clone();
                move |request: Request<Body>| {
                    let sent = sent.clone();
                    async move {
                        let mut body: Value = serde_json::from_slice(
                            &request.into_body().collect().await.unwrap().to_bytes(),
                        )
                        .unwrap();
                        if body["type"] == "text" && body.get("media_type").is_none() {
                            body["media_type"] = "text/plain".into();
                        }
                        body["index"] = json!(2);
                        sent.send("fast").unwrap();
                        Ok::<_, std::convert::Infallible>(http::Response::new(
                            axum::body::Body::from(body.to_string()),
                        ))
                    }
                }
            }),
        )
        .await
        .unwrap();
    let _slow = scopeguard::guard(tokio::spawn(slow_listener), |task| task.abort());
    let _fast = scopeguard::guard(tokio::spawn(fast_listener), |task| task.abort());
    let data = root.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let repo = std::path::PathBuf::from(
        std::env::var_os("PISHOO_MESSAGE_REPO").expect("set PISHOO_MESSAGE_REPO"),
    );
    {
        let db = rusqlite::Connection::open(data.join("database")).unwrap();
        db.execute_batch(&std::fs::read_to_string(repo.join("src/sql/schema.sql")).unwrap())
            .unwrap();
        db.execute_batch(
            &std::fs::read_to_string(repo.join("src/sql/migrations/v5_add_message_changes.sql"))
                .unwrap(),
        )
        .unwrap();
        db.execute_batch(
            &std::fs::read_to_string(repo.join("src/sql/migrations/v6_add_image_content.sql"))
                .unwrap(),
        )
        .unwrap();
        for id in 1..=if large_image || cancel_early { 1 } else { 34 } {
            let target = if !large_image && id == 1 {
                "slow.dhttp.net"
            } else {
                "fast.dhttp.net"
            };
            db.execute("INSERT INTO chat_messages(id,contact_name,client_message_id,direction,state,sender_name,recipient_name,text,sender_subject,recipient_subject,created_at,updated_at) VALUES(?1,?2,?1,'outgoing','queued','alice.dhttp.net',?2,?5,?3,?4,1,1)",rusqlite::params![id,target,local_hash,peers[target],"你".repeat(4000)]).unwrap();
            db.execute("INSERT INTO chat_jobs(id,kind,contact_name,message_id,state,available_at,created_at,updated_at) VALUES(?1,'send',?2,?1,'queued',1,1,1)",rusqlite::params![id,target]).unwrap();
            if large_image {
                let mut image = vec![0; 512 * 1024];
                image[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
                let content = format!(
                    "data:image/png;base64,{}",
                    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, image)
                );
                db.execute("UPDATE chat_messages SET text='[image]',media_type='image/png',image_content=?1 WHERE id=?2",rusqlite::params![content,id]).unwrap();
            }
        }
    }
    let access = Arc::new(
        access_control::AccessService::load_from_db(
            "sqlite::memory:",
            endpoint.name(),
            &access_control::SubjectId::new(local_hash.as_bytes()).unwrap(),
        )
        .await
        .unwrap(),
    );
    let api_peers = peers.clone();
    let api=axum::Router::new().route("/std/contact",axum::routing::get(move |axum::extract::Query(query):axum::extract::Query<std::collections::HashMap<String,String>>| {
        let peers=api_peers.clone();
        async move { axum::Json(json!({"name":query["name"],"status":"active","subject_id":peers[&query["name"]]})) }
    }));
    let bytes = std::fs::read(std::env::var_os("PISHOO_MESSAGE_COMPONENT").unwrap()).unwrap();
    let lib = load(&bytes, &data);
    assert!(
        !lib.component
            .component_type()
            .exports(&lib.runtime.engine)
            .any(|(name, _)| name.starts_with("wasi:cli/run@")),
        "Message must not export a background CLI entry"
    );
    let tasks = TaskTracker::new();
    let outgoing = || Outgoing::Granted {
        endpoint: endpoint.clone(),
        access: access.clone(),
        api: api.clone(),
    };
    let local = endpoint.local_authority().unwrap().clone();
    let owner = journal::journal_remote(&local, &local);
    #[expect(
        clippy::async_yields_async,
        reason = "return the reader task so the test can cancel the subscription"
    )]
    let subscribe = || async {
        let invocation = Invocation::new(
            lib.clone(),
            endpoint.clone(),
            &dhttp::HandshakeSummary {
                alpn: None,
                local: Some(local.clone()),
                remote: Some(owner.clone()),
            },
            tasks.clone(),
            outgoing(),
        )
        .unwrap();
        let request = Request::get("https://alice.dhttp.net/comm/message/list")
            .header("accept", "text/event-stream")
            .body(empty())
            .unwrap();
        let response = invocation.execute(request).await.unwrap();
        assert_eq!(response.status(), http::StatusCode::OK);
        tokio::spawn(async move {
            let mut body = response.into_body();
            while let Some(frame) = body.frame().await {
                frame.unwrap();
            }
        })
    };
    // Queued records alone do not cause delivery without a live subscription.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(received.try_recv().is_err());
    let first = scopeguard::guard(subscribe().await, |task| task.abort());
    let second = scopeguard::guard(subscribe().await, |task| task.abort());
    if cancel_early {
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), received.recv())
                .await
                .unwrap()
                .unwrap(),
            "slow-started"
        );
        drop(first);
        drop(second);
        tokio::time::timeout(Duration::from_secs(5), async {
            while !tasks.is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let db = rusqlite::Connection::open_with_flags_and_vfs(
            data.join("database"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
            "unix-dotfile",
        )
        .unwrap();
        db.busy_timeout(Duration::ZERO).unwrap();
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM chat_jobs WHERE lease_token IS NOT NULL",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            db.query_row("SELECT state FROM chat_messages WHERE id=1", [], |r| r
                .get::<_, String>(
                0
            ))
            .unwrap(),
            "queued"
        );
        let resumed = scopeguard::guard(subscribe().await, |task| task.abort());
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                if db
                    .query_row("SELECT state FROM chat_messages WHERE id=1", [], |r| {
                        r.get::<_, String>(0)
                    })
                    .ok()
                    .as_deref()
                    == Some("sent")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        drop(resumed);
        reaped(&tasks).await;
        assert!(!data.join("database.lock").exists());
        return;
    }
    let started = std::time::Instant::now();
    let mut fast_count = 0;
    let mut slow_started = false;
    while fast_count < if large_image { 1 } else { 3 } || (!large_image && !slow_started) {
        match tokio::time::timeout(Duration::from_secs(10), received.recv())
            .await
            .unwrap()
            .unwrap()
        {
            "slow-started" => slow_started = true,
            "fast" => fast_count += 1,
            "slow-finished" if !large_image => panic!("slow peer blocked ready fast messages"),
            _ => unreachable!(),
        }
    }
    println!(
        "MESSAGE_SUBSCRIPTION_FAST_DELIVERY_MS={}",
        started.elapsed().as_millis()
    );
    let db = rusqlite::Connection::open_with_flags_and_vfs(
        data.join("database"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
        "unix-dotfile",
    )
    .unwrap();
    // Do not block this single-threaded async test while a guest is yielding
    // inside its short transaction; retry observation asynchronously instead.
    db.busy_timeout(Duration::ZERO).unwrap();
    let expected = if large_image { 1 } else { 34 };
    let settled = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let count = db.query_row(
                "SELECT count(*) FROM chat_messages WHERE state='sent'",
                [],
                |r| r.get::<_, i64>(0),
            );
            if count.ok() == Some(expected) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    if settled.is_err() {
        let diagnostic = rusqlite::Connection::open(data.join("database")).unwrap();
        let states = diagnostic.prepare("SELECT state,count(*),coalesce(error_message,'') FROM chat_messages GROUP BY state,error_message").unwrap()
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?))).unwrap()
            .collect::<rusqlite::Result<Vec<_>>>().unwrap();
        panic!(
            "subscription delivery did not settle: {states:?}; tasks={}, readers={}/{}",
            tasks.len(),
            first.is_finished(),
            second.is_finished()
        );
    }
    while let Ok(event) = received.try_recv() {
        if event == "fast" {
            fast_count += 1;
        }
    }
    assert_eq!(
        fast_count,
        if large_image { 1 } else { 33 },
        "two subscriptions must not duplicate delivery"
    );
    drop(first);
    drop(second);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !tasks.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM chat_jobs WHERE lease_token IS NOT NULL",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    // New queue work remains untouched after the last connection closes.
    db.execute("INSERT INTO chat_messages(id,contact_name,client_message_id,direction,state,sender_name,recipient_name,text,sender_subject,recipient_subject,created_at,updated_at) VALUES(100,'fast.dhttp.net','resume','outgoing','queued','alice.dhttp.net','fast.dhttp.net','after closing',?1,?2,1,1)",rusqlite::params![local_hash,peers["fast.dhttp.net"]]).unwrap();
    db.execute("INSERT INTO chat_jobs(id,kind,contact_name,message_id,state,available_at,created_at,updated_at) VALUES(100,'send','fast.dhttp.net',100,'queued',1,1,1)",[]).unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(received.try_recv().is_err());
    assert_eq!(
        db.query_row("SELECT state FROM chat_messages WHERE id=100", [], |r| {
            r.get::<_, String>(0)
        })
        .unwrap(),
        "queued"
    );
    let resumed = scopeguard::guard(subscribe().await, |task| task.abort());
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), received.recv())
            .await
            .unwrap()
            .unwrap(),
        "fast"
    );
    drop(resumed);
    reaped(&tasks).await;
    assert!(!data.join("database.lock").exists());
}

fn process_usage() -> (u64, f64) {
    let output = std::process::Command::new("ps")
        .args([
            "-o",
            "rss=",
            "-o",
            "time=",
            "-p",
            &std::process::id().to_string(),
        ])
        .output()
        .unwrap();
    let text = String::from_utf8(output.stdout).unwrap();
    let mut parts = text.split_whitespace();
    let rss = parts.next().unwrap().parse::<u64>().unwrap();
    let cpu = parts
        .next()
        .unwrap()
        .split(':')
        .fold(0.0, |sum, value| sum * 60.0 + value.parse::<f64>().unwrap());
    (rss, cpu)
}

#[tokio::test]
#[ignore = "opt-in 1/10/50 idle subscription measurement; stops expanding at 512 MiB process RSS"]
async fn message_chat_idle_subscription_resource_samples() {
    let bytes = std::fs::read(std::env::var_os("PISHOO_MESSAGE_COMPONENT").unwrap()).unwrap();
    let root = tempfile::tempdir().unwrap();
    let (local, owner, _) = journal::journal_credentials(root.path()).await;
    let lib = load(&bytes, &root.path().join("data"));
    for requested in [1, 10, 50] {
        let tasks = TaskTracker::new();
        let mut streams = Vec::new();
        for _ in 0..requested {
            if process_usage().0 > 512 * 1024 {
                break;
            }
            let invocation = Invocation::new(
                lib.clone(),
                crate::test_support::identity::endpoint("alice.dhttp.net"),
                &dhttp::HandshakeSummary {
                    alpn: None,
                    local: Some(local.clone()),
                    remote: Some(owner.clone()),
                },
                tasks.clone(),
                Outgoing::Denied,
            )
            .unwrap();
            let mut request = Request::get("https://alice.dhttp.net/comm/message/list")
                .body(empty())
                .unwrap();
            request
                .headers_mut()
                .insert("accept", "text/event-stream".parse().unwrap());
            let response = invocation.execute(request).await.unwrap();
            assert_eq!(response.status(), http::StatusCode::OK);
            let mut body = response.into_body();
            assert!(
                event(&mut body, &mut Vec::new())
                    .await
                    .starts_with("event: ready")
            );
            streams.push(body);
        }
        let (rss_before, cpu_before) = process_usage();
        let started = std::time::Instant::now();
        tokio::time::sleep(Duration::from_secs(3)).await;
        let (rss_after, cpu_after) = process_usage();
        println!(
            "MESSAGE_IDLE requested={requested} opened={} rss_before_kib={rss_before} rss_after_kib={rss_after} cpu_seconds={:.3} wall_seconds={:.3}",
            streams.len(),
            cpu_after - cpu_before,
            started.elapsed().as_secs_f64()
        );
        let opened = streams.len();
        let dropped = std::time::Instant::now();
        drop(streams);
        reaped(&tasks).await;
        println!(
            "MESSAGE_IDLE_REAP opened={opened} milliseconds={}",
            dropped.elapsed().as_millis()
        );
        if opened < requested || rss_after > 512 * 1024 {
            break;
        }
    }
}
