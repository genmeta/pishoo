//! Optional integration with the separately built Message product; no bundled business source.
use super::*;

async fn open(
    lib: Arc<Lib>,
    tasks: &TaskTracker,
    local: &dhttp::LocalAuthority,
    remote: Option<dhttp::RemoteAuthority>,
    path: &str,
) -> http::Response<Body> {
    Invocation {
        lib,
        local: local.clone(),
        remote,
        tasks: tasks.clone(),
        outgoing: Outgoing::Denied,
    }
    .execute(
        Request::get(format!("https://alice.dhttp.net{path}"))
            .header(http::header::ACCEPT, "text/event-stream")
            .body(empty())
            .unwrap(),
    )
    .await
    .unwrap()
}

async fn event(body: &mut Body, contains: &str) -> String {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut text = String::new();
        while !text.contains(contains) || !text.ends_with("\n\n") {
            let frame = body.frame().await.expect("stream ended").unwrap();
            if let Ok(data) = frame.into_data() {
                text.push_str(std::str::from_utf8(&data).unwrap());
            }
        }
        text
    })
    .await
    .unwrap()
}

async fn heartbeat(body: &mut Body) {
    tokio::time::timeout(Duration::from_secs(20), async {
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\n\n") {
            let frame = body.frame().await.expect("stream ended").unwrap();
            if let Ok(data) = frame.into_data() {
                bytes.extend_from_slice(&data);
            }
        }
        assert_eq!(
            bytes, b":\n\n",
            "idle transport must not query/push unnotified DB changes"
        );
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "run alone with PISHOO_MESSAGE_WASM; generates isolated TLS credentials"]
async fn real_message_stream_replays_state_changes_and_releases_sqlite() {
    let credentials = tempfile::tempdir().unwrap();
    let (local, owner, peer) = super::journal::journal_credentials(credentials.path()).await;
    let directory = tempfile::tempdir().unwrap();
    let bytes = std::fs::read(
        std::env::var("PISHOO_MESSAGE_WASM").expect("build Message wasm32-wasip2 first"),
    )
    .unwrap();
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let lib = Arc::new(
        Lib::load(
            runtime,
            "dev.genmeta.tech.dhttp.net/message".into(),
            &component(&bytes),
            directory.path(),
        )
        .unwrap(),
    );
    let tasks = TaskTracker::new();
    for (path, accept, status) in [
        ("/message/list", "application/json", 200),
        ("/message/list", "text/event-stream;q=0", 200),
        ("/message/list?cursor=bad", "application/json", 400),
        ("/message/events", "text/event-stream", 404),
    ] {
        let reply = Invocation {
            lib: lib.clone(),
            local: local.clone(),
            remote: Some(owner.clone()),
            tasks: tasks.clone(),
            outgoing: Outgoing::Denied,
        }
        .execute(
            Request::get(format!("https://alice.dhttp.net{path}"))
                .header(http::header::ACCEPT, accept)
                .body(empty())
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(reply.status().as_u16(), status);
        assert_eq!(reply.headers()["content-type"], "application/json");
        assert_eq!(reply.headers()["vary"], "Accept");
        reply.into_body().collect().await.unwrap();
    }
    for caller in [None, Some(peer)] {
        let denied = open(lib.clone(), &tasks, &local, caller, "/message/list").await;
        assert_eq!(denied.status(), http::StatusCode::FORBIDDEN);
        denied.into_body().collect().await.unwrap();
    }
    let mut response = open(
        lib.clone(),
        &tasks,
        &local,
        Some(owner.clone()),
        "/message/list",
    )
    .await;
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let ready = event(response.body_mut(), "event: ready").await;
    let start = ready
        .lines()
        .find_map(|line| line.strip_prefix("id: "))
        .unwrap()
        .to_owned();
    let db = rusqlite::Connection::open_with_flags_and_vfs(
        directory.path().join("database"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
        "unix-dotfile",
    )
    .unwrap();
    db.busy_timeout(Duration::from_secs(2)).unwrap();
    db.execute("INSERT INTO chat_messages(id,contact_name,client_message_id,direction,state,sender_name,recipient_name,text,created_at,updated_at) VALUES(1,'bob.dhttp.net','stream-probe','outgoing','queued','alice.dhttp.net','bob.dhttp.net','hello stream',1,1)",[]).unwrap();
    lib.changes.send_modify(|n| *n += 1);
    let message = event(response.body_mut(), "event: messages").await;
    assert!(message.contains("hello stream"));
    assert!(message.contains("queued"));
    let mut cursor = message
        .lines()
        .find_map(|line| line.strip_prefix("id: "))
        .unwrap()
        .to_owned();
    assert_ne!(cursor, start);
    assert!(!message.starts_with(':'));
    db.execute(
        "UPDATE chat_messages SET state='sending',updated_at=2 WHERE id=1",
        [],
    )
    .unwrap();
    lib.changes.send_modify(|n| *n += 1);
    let patch = event(response.body_mut(), "event: messages").await;
    assert!(patch.contains("\"patch\""));
    assert!(patch.contains("sending"));
    assert!(!patch.contains("hello stream"));
    assert!(!patch.starts_with(':'));
    cursor = patch
        .lines()
        .find_map(|line| line.strip_prefix("id: "))
        .unwrap()
        .to_owned();
    drop(response);
    // This write succeeds while there is no reader: no stream holds a read tx.
    db.execute("UPDATE chat_messages SET state='sent' WHERE id=1", [])
        .unwrap();
    let mut resumed = open(
        lib.clone(),
        &tasks,
        &local,
        Some(owner.clone()),
        &format!("/message/list?cursor={cursor}"),
    )
    .await;
    let replay = event(resumed.body_mut(), "event: messages").await;
    assert!(replay.contains("sent"));
    assert!(replay.contains("hello stream"));
    assert!(!replay.contains("\"patch\""));
    assert!(replay.contains("\"index\":1"));
    // Wait until the connection is in its idle loop, then mutate durable
    // state without notifying. A second heartbeat must NOT query that state.
    heartbeat(resumed.body_mut()).await;
    db.execute("UPDATE chat_messages SET state='failed' WHERE id=1", [])
        .unwrap();
    heartbeat(resumed.body_mut()).await;
    lib.changes.send_modify(|n| *n += 1);
    let notified = event(resumed.body_mut(), "event: messages").await;
    assert!(notified.contains("failed"));
    lib.stream_retired.cancel();
    reaped(&tasks).await;
    drop(resumed);
    db.execute("UPDATE chat_messages SET state='failed' WHERE id=1", [])
        .unwrap();
    assert_eq!(
        db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        5
    );
}
