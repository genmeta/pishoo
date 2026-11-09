//! Opt-in end-to-end native file and Message tests with actual WASM components.
use serde_json::{Value, json};

use super::*;

fn protocol(doc: Value) -> Request<Body> {
    Request::post("https://alice.dhttp.net/comm/message")
        .header("content-type", "application/json")
        .body(
            Full::new(Bytes::from(doc.to_string()))
                .map_err(|never| match never {})
                .boxed_unsync(),
        )
        .unwrap()
}

fn chunk(id: &str, offset: usize, bytes: &[u8], checksum: Option<&str>) -> Request<Body> {
    let mut doc = json!({"type":"upload_chunk","id":id,"offset":offset});
    if let Some(checksum) = checksum {
        doc["checksum"] = checksum.into();
    }
    let mut body = format!("--file-test-boundary\r\nContent-Disposition: form-data; name=\"message\"\r\n\r\n{doc}\r\n--file-test-boundary\r\nContent-Disposition: form-data; name=\"chunk\"; filename=\"chunk\"\r\nContent-Type: application/octet-stream\r\n\r\n").into_bytes();
    body.extend_from_slice(bytes);
    body.extend_from_slice(b"\r\n--file-test-boundary--\r\n");
    Request::post("https://alice.dhttp.net/comm/message")
        .header(
            "content-type",
            "multipart/form-data; boundary=file-test-boundary",
        )
        .body(
            Full::new(Bytes::from(body))
                .map_err(|never| match never {})
                .boxed_unsync(),
        )
        .unwrap()
}

#[tokio::test]
#[ignore = "run alone with PISHOO_MESSAGE_COMPONENT and PISHOO_FILE_PROBE_COMPONENT"]
async fn native_files_real_wasm_scope_and_message_authorization() {
    let root = tempfile::tempdir().unwrap();
    let (local, owner, bob) = journal::journal_credentials(root.path()).await;
    let profile =
        dhttp_home::identity::IdentityProfile::try_from(root.path().join("alice")).unwrap();
    let endpoint = dhttp::Endpoint::new(
        qbase::endpoint::Endpoint::new(
            "alice.dhttp.net",
            profile.load_certs().await.unwrap(),
            profile.load_key().await.unwrap(),
            profile.load_ocsp().await.unwrap(),
        )
        .unwrap(),
    );
    let hash = |authority: &dhttp::LocalAuthority| {
        dhttp_home::certificate::extract_dhttp_subject_key_identifier(authority.certificates())
            .unwrap()
            .owner_hash()
            .to_string()
    };
    let bob_hash =
        dhttp_home::certificate::extract_dhttp_subject_key_identifier(bob.certificates())
            .unwrap()
            .owner_hash()
            .to_string();
    let access = Arc::new(
        access_control::AccessService::load_from_db(
            "sqlite::memory:",
            endpoint.name(),
            &access_control::SubjectId::new(hash(&local).as_bytes()).unwrap(),
        )
        .await
        .unwrap(),
    );
    std::fs::create_dir(root.path().join("file")).unwrap();
    let api = axum::Router::new()
        .route(
            "/std/contact",
            axum::routing::get(move || {
                let hash = bob_hash.clone();
                async move {
                    axum::Json(json!({"name":"bob.dhttp.net","status":"active","subject_id":hash}))
                }
            }),
        )
        .merge(crate::file::library_router(
            root.path().join("file"),
            endpoint.clone(),
        ))
        .layer(axum::middleware::from_fn_with_state(
            access.clone(),
            crate::routes::authorize,
        ));
    let tasks = TaskTracker::new();
    let invoke = |lib: Arc<Lib>, remote: Option<dhttp::RemoteAuthority>| {
        Invocation::new(
            lib,
            endpoint.clone(),
            &dhttp::HandshakeSummary {
                alpn: None,
                local: Some(local.clone()),
                remote,
            },
            tasks.clone(),
            Outgoing::Granted {
                endpoint: endpoint.clone(),
                access: access.clone(),
                api: api.clone(),
            },
        )
        .unwrap()
    };
    let req = |method: &str, path: &str, headers: Vec<(&str, String)>, body: Vec<u8>| {
        let mut r = Request::builder()
            .method(method)
            .uri(format!("https://alice.dhttp.net{path}"));
        for (k, v) in headers {
            r = r.header(k, v);
        }
        r.body(
            Full::new(Bytes::from(body))
                .map_err(|never| match never {})
                .boxed_unsync(),
        )
        .unwrap()
    };
    let probe = std::fs::read(std::env::var_os("PISHOO_FILE_PROBE_COMPONENT").unwrap()).unwrap();
    let first = load(&probe, &root.path().join("first"));
    let second = load(&probe, &root.path().join("second"));
    let response = invoke(first.clone(), None)
        .execute(req(
            "POST",
            "/std/file/upload?request_id=scope&directory=peer/date",
            vec![
                ("tus-resumable", "1.0.0".into()),
                ("upload-length", "3".into()),
            ],
            vec![],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 201);
    let location = response.headers()["location"].to_str().unwrap().to_owned();
    let id = location.split("id=").nth(1).unwrap();
    response.into_body().collect().await.unwrap();
    let response = invoke(first.clone(), None)
        .execute(req(
            "PATCH",
            &location,
            vec![
                ("tus-resumable", "1.0.0".into()),
                ("upload-offset", "0".into()),
                ("content-type", "application/offset+octet-stream".into()),
            ],
            b"abc".to_vec(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    response.into_body().collect().await.unwrap();
    for (method, path) in [
        ("GET", format!("/std/file?id={id}")),
        ("HEAD", location.clone()),
        ("DELETE", location.clone()),
        ("PATCH", location.clone()),
        ("GET", format!("/std/file?id={id}&view=meta")),
    ] {
        eprintln!("scope probe {method} {path}");
        let response = invoke(second.clone(), None)
            .execute(req(
                method,
                &path,
                vec![
                    ("tus-resumable", "1.0.0".into()),
                    ("x-lib", "first".into()),
                    ("range", "bytes=0-1".into()),
                ],
                vec![],
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), 404);
        response.into_body().collect().await.unwrap();
    }
    let response = invoke(first.clone(), None)
        .execute(req("GET", &format!("/std/file?id={id}"), vec![], vec![]))
        .await
        .unwrap();
    assert_eq!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .as_ref(),
        b"abc"
    );
    // The host copies only this invocation's source, and the personal file is
    // independent from both the source bytes and its native resource index.
    let response = invoke(first.clone(), Some(owner.clone()))
        .execute(req(
            "POST",
            &format!("/std/file/copy?id={id}&directory=Downloads"),
            vec![],
            vec![],
        ))
        .await
        .unwrap();
    let status = response.status();
    let receipt: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(status, 200, "{receipt}");
    let saved = root
        .path()
        .join("file")
        .join(receipt["path"].as_str().unwrap());
    assert_eq!(std::fs::read(&saved).unwrap(), b"abc");
    std::fs::write(&saved, b"personal edit").unwrap();
    let response = invoke(first.clone(), None)
        .execute(req("GET", &format!("/std/file?id={id}"), vec![], vec![]))
        .await
        .unwrap();
    assert_eq!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .as_ref(),
        b"abc"
    );
    let response = invoke(second.clone(), Some(owner.clone()))
        .execute(req(
            "POST",
            &format!("/std/file/copy?id={id}&directory=Downloads"),
            vec![],
            vec![],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    response.into_body().collect().await.unwrap();
    let bytes = std::fs::read(std::env::var_os("PISHOO_MESSAGE_COMPONENT").unwrap()).unwrap();
    let message = load(&bytes, &root.path().join("message"));
    let creation = json!({"type":"upload","peer":"bob","message_id":"file-test","request_id":"file-test","name":"test.txt","media_type":"text/plain","size":3});
    let response = invoke(message.clone(), Some(bob.clone()))
        .execute(protocol(creation.clone()))
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    response.into_body().collect().await.unwrap();
    let response = invoke(message.clone(), Some(owner.clone()))
        .execute(protocol(creation.clone()))
        .await
        .unwrap();
    let status = response.status();
    assert!(response.headers().get("location").is_none());
    assert!(response.headers().get("tus-resumable").is_none());
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(status, 201, "{body:?}");
    let created: Value = serde_json::from_slice(&body).unwrap();
    let id = created["id"].as_str().unwrap();
    let day = created["day"].as_str().unwrap();
    // Repeated creation returns the same file and archive date.
    let response = invoke(message.clone(), Some(owner.clone()))
        .execute(protocol(creation))
        .await
        .unwrap();
    assert_eq!(response.status(), 201);
    let repeated: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(repeated, created);
    for (path, body) in [
        (
            "/comm/message?action=upload.status",
            json!({"type":"upload_status","id":id}),
        ),
        (
            "/comm/message?file_id=old",
            json!({"type":"file_info","id":id}),
        ),
        (
            "/comm/message",
            json!({"type":"upload_status","id":id,"extra":true}),
        ),
        (
            "/comm/message",
            json!({"from":"alice","to":"bob","content":"old","client_message_id":"old","media_type":"text/plain"}),
        ),
    ] {
        let response = invoke(message.clone(), Some(owner.clone()))
            .execute(req(
                "POST",
                path,
                vec![("content-type", "application/json".into())],
                serde_json::to_vec(&body).unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            400,
            "old query and untyped messages must not be accepted"
        );
        response.into_body().collect().await.unwrap();
    }
    // Body control messages are owner-only, even when a contact knows the ID.
    for doc in [
        json!({"type":"upload_status","id":id}),
        json!({"type":"upload_cancel","id":id}),
        json!({"type":"download","index":1}),
        json!({"type":"file_save","index":1,"directory":"Downloads"}),
    ] {
        let response = invoke(message.clone(), Some(bob.clone()))
            .execute(protocol(doc))
            .await
            .unwrap();
        assert_eq!(response.status(), 403);
        response.into_body().collect().await.unwrap();
    }
    for kind in ["file_read", "file_info"] {
        let mut request = protocol(json!({"type":kind,"id":id}));
        request
            .headers_mut()
            .insert("if-none-match", "*".parse().unwrap());
        let response = invoke(message.clone(), Some(bob.clone()))
            .execute(request)
            .await
            .unwrap();
        assert_eq!(response.status(), 409, "incomplete files are not shared");
        response.into_body().collect().await.unwrap();
    }
    // Bad checksum and stale offsets cannot advance the native prefix.
    for (offset, checksum, expected) in [
        (
            0,
            Some("sha256 AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="),
            460,
        ),
        (1, None, 409),
    ] {
        let response = invoke(message.clone(), Some(owner.clone()))
            .execute(chunk(id, offset, b"abc", checksum))
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        response.into_body().collect().await.unwrap();
    }
    let response = invoke(message.clone(), Some(owner.clone()))
        .execute(protocol(json!({"type":"upload_status","id":id})))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let progress: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(progress["offset"], 0);
    for (offset, bytes) in [(0, b"a".as_slice()), (1, b"bc".as_slice())] {
        let response = invoke(message.clone(), Some(owner.clone()))
            .execute(chunk(id, offset, bytes, None))
            .await
            .unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(status, 200, "{body:?}");
        let progress: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(progress["offset"], offset + bytes.len());
    }
    let response = invoke(message.clone(), Some(bob.clone()))
        .execute(protocol(json!({"type":"file_read","id":id})))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .as_ref(),
        b"abc"
    );
    for (kind, headers, expected) in [
        ("file_info", vec![], 200),
        ("file_read", vec![("range", "bytes=1-2")], 206),
        ("file_read", vec![("if-none-match", "*")], 304),
    ] {
        let mut request = protocol(json!({"type":kind,"id":id}));
        for (key, value) in headers {
            request.headers_mut().insert(key, value.parse().unwrap());
        }
        let response = invoke(message.clone(), Some(bob.clone()))
            .execute(request)
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        response.into_body().collect().await.unwrap();
    }
    let descriptor = json!({"version":2,"source_id":id,"name":"test.txt","type":"text/plain","size":3,"blake3":blake3::hash(b"abc").to_hex().to_string(),"day":day});
    let doc = json!({"type":"file","from":"alice.dhttp.net","to":"bob.dhttp.net","client_message_id":"file-test","content":descriptor.to_string()});
    let mut invalid_doc = doc.clone();
    invalid_doc["client_message_id"] = "wrong-message".into();
    let response = invoke(message.clone(), Some(owner.clone()))
        .execute(protocol(invalid_doc))
        .await
        .unwrap();
    assert_eq!(response.status(), 409);
    response.into_body().collect().await.unwrap();
    let response = invoke(message.clone(), Some(bob.clone()))
        .execute(protocol(json!({"type":"file_read","id":id})))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        200,
        "failed submission does not revoke directory sharing"
    );
    assert_eq!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .as_ref(),
        b"abc"
    );
    let response = invoke(message.clone(), Some(owner.clone()))
        .execute(protocol(doc))
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(status, 202, "{body:?}");
    let message_index = serde_json::from_slice::<Value>(&body).unwrap()["index"]
        .as_i64()
        .unwrap();
    for (caller, expected) in [(owner.clone(), 200), (bob.clone(), 403)] {
        let response = invoke(message.clone(), Some(caller))
            .execute(protocol(
                json!({"type":"file_save","index":message_index,"directory":"Documents"}),
            ))
            .await
            .unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(status, expected, "{body:?}");
    }
    assert_eq!(
        std::fs::read(root.path().join("file/Documents/test.txt")).unwrap(),
        b"abc"
    );
    for old in [
        "/message",
        "/message/file",
        "/comm/message/file",
        "/comm/message/upload",
    ] {
        let response = invoke(message.clone(), Some(owner.clone()))
            .execute(req("POST", old, vec![], vec![]))
            .await
            .unwrap();
        assert_eq!(response.status(), 404, "{old}");
        response.into_body().collect().await.unwrap();
    }
    let mut request = protocol(json!({"type":"file_read","id":id}));
    request
        .headers_mut()
        .insert("range", "bytes=1-2".parse().unwrap());
    let response = invoke(message.clone(), Some(bob.clone()))
        .execute(request)
        .await
        .unwrap();
    assert_eq!(response.status(), 206);
    assert_eq!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .as_ref(),
        b"bc"
    );
    let response = invoke(message.clone(), Some(owner.clone()))
        .execute(protocol(json!({"type":"upload_cancel","id":id})))
        .await
        .unwrap();
    assert_eq!(response.status(), 409);
    response.into_body().collect().await.unwrap();
    // Real host ACL runs before body type dispatch, including conditional reads.
    let protected = axum::Router::new()
        .route(
            "/std/comm/message",
            axum::routing::any({
                let lib = message.clone();
                let endpoint = endpoint.clone();
                let tasks = tasks.clone();
                let access = access.clone();
                let api = api.clone();
                move |mut request: Request<axum::body::Body>| {
                    let handshake = request
                        .extensions()
                        .get::<dhttp::HandshakeSummary>()
                        .unwrap()
                        .clone();
                    let invocation = Invocation::new(
                        lib.clone(),
                        endpoint.clone(),
                        &handshake,
                        tasks.clone(),
                        Outgoing::Granted {
                            endpoint: endpoint.clone(),
                            access: access.clone(),
                            api: api.clone(),
                        },
                    )
                    .unwrap();
                    *request.uri_mut() = format!(
                        "https://alice.dhttp.net{}",
                        request
                            .uri()
                            .path_and_query()
                            .unwrap()
                            .as_str()
                            .strip_prefix("/std")
                            .unwrap()
                    )
                    .parse()
                    .unwrap();
                    async move {
                        invocation
                            .execute(request.map(|body| body.map_err(Into::into).boxed_unsync()))
                            .await
                            .unwrap()
                            .map(axum::body::Body::new)
                    }
                }
            }),
        )
        .layer(axum::middleware::from_fn_with_state(
            access.clone(),
            crate::routes::authorize,
        ));
    let check = || {
        Request::post("/std/comm/message")
            .header("content-type", "application/json")
            .header(
                "if-none-match",
                format!("\"blake3-{}\"", blake3::hash(b"abc").to_hex()),
            )
            .extension(dhttp::HandshakeSummary {
                alpn: None,
                local: Some(local.clone()),
                remote: Some(bob.clone()),
            })
            .body(axum::body::Body::from(
                json!({"type":"file_read","id":id}).to_string(),
            ))
            .unwrap()
    };
    use tower::ServiceExt;
    assert_eq!(
        protected.clone().oneshot(check()).await.unwrap().status(),
        403
    );
    access
        .set_policy(
            "POST".parse().unwrap(),
            "/std/message",
            access_control::Effect::Allow,
            "**".parse().unwrap(),
        )
        .await
        .unwrap();
    access
        .set_policy(
            "PUT".parse().unwrap(),
            "/std/comm/message",
            access_control::Effect::Allow,
            "**".parse().unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        protected.clone().oneshot(check()).await.unwrap().status(),
        403,
        "old path and method grants must not authorize the new POST"
    );
    access
        .set_policy(
            "POST".parse().unwrap(),
            "/std/comm/message",
            access_control::Effect::Allow,
            "**".parse().unwrap(),
        )
        .await
        .unwrap();
    let conditional = protected.clone().oneshot(check()).await.unwrap();
    assert_eq!(conditional.status(), 304);
    assert!(
        conditional
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .is_empty()
    );
    access
        .set_policy(
            "POST".parse().unwrap(),
            "/std/comm/message",
            access_control::Effect::Deny,
            "**".parse().unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(protected.oneshot(check()).await.unwrap().status(), 403);
    // 100 MiB crosses the 20 MiB per-outgoing-response budget. Message must
    // stream bounded native ranges without accumulating the whole file.
    let length = 100 * 1024 * 1024usize;
    let block = vec![0x5au8; 4 * 1024 * 1024];
    let response = invoke(message.clone(), Some(owner.clone())).execute(protocol(json!({"type":"upload","peer":"bob","message_id":"large-file","request_id":"large-file","name":"large.bin","media_type":"application/octet-stream","size":length}))).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(status, 201, "{body:?}");
    let created: Value = serde_json::from_slice(&body).unwrap();
    let large_day = created["day"].as_str().unwrap();
    let large_id = created["id"].as_str().unwrap();
    let mut digest = blake3::Hasher::new();
    for offset in (0..length).step_by(block.len()) {
        let response = invoke(message.clone(), Some(owner.clone()))
            .execute(chunk(large_id, offset, &block, None))
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(status, 200, "large chunk offset {offset}: {bytes:?}");
        digest.update(&block);
    }
    let large_desc = json!({"version":2,"source_id":large_id,"name":"large.bin","type":"application/octet-stream","size":length,"blake3":digest.finalize().to_hex().to_string(),"day":large_day});
    let large_doc = json!({"type":"file","from":"alice.dhttp.net","to":"bob.dhttp.net","client_message_id":"large-file","content":large_desc.to_string()});
    let response = invoke(message.clone(), Some(owner.clone()))
        .execute(protocol(large_doc))
        .await
        .unwrap();
    assert_eq!(response.status(), 202);
    response.into_body().collect().await.unwrap();
    let response = invoke(message.clone(), Some(bob.clone()))
        .execute(protocol(json!({"type":"file_read","id":large_id})))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let mut body = response.into_body();
    let mut actual = blake3::Hasher::new();
    let mut count = 0;
    while let Some(frame) = body.frame().await {
        let frame = frame.unwrap();
        if let Some(bytes) = frame.data_ref() {
            count += bytes.len();
            actual.update(bytes);
        }
    }
    assert_eq!(count, length);
    assert_eq!(actual.finalize(), digest.finalize());
    let response = invoke(message.clone(), Some(owner.clone())).execute(protocol(json!({"type":"upload","peer":"bob","message_id":"cancel-file","request_id":"cancel-file","name":"empty.txt","media_type":"text/plain","size":0}))).await.unwrap();
    assert_eq!(response.status(), 201);
    let created: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let cancelled_id = created["id"].as_str().unwrap();
    let response = invoke(message.clone(), Some(owner.clone()))
        .execute(protocol(json!({"type":"upload_cancel","id":cancelled_id})))
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    response.into_body().collect().await.unwrap();
    let response = invoke(message.clone(), Some(bob.clone()))
        .execute(protocol(json!({"type":"file_read","id":cancelled_id})))
        .await
        .unwrap();
    assert_eq!(response.status(), 410);
    response.into_body().collect().await.unwrap();
    // A received descriptor creates no native file or local download operation.
    let incoming = json!({"type":"file","from":"bob.dhttp.net","to":"alice.dhttp.net","client_message_id":"received-file","content":descriptor.to_string()});
    let response = invoke(message.clone(), Some(bob.clone()))
        .execute(protocol(incoming))
        .await
        .unwrap();
    assert_eq!(response.status(), 201);
    let _: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    reaped(&tasks).await;
    let db = rusqlite::Connection::open_with_flags_and_vfs(
        root.path().join("message/database"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
        "unix-dotfile",
    )
    .unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM attachment_downloads", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    // Pre-native completed attachments stay readable through the new business
    // POST without rewriting stored messages or moving their historical bytes.
    let legacy = json!({"version":1,"name":"historical.txt","type":"text/plain","size":3,"blake3":blake3::hash(b"old").to_hex().to_string(),"day":"2026-11-01"});
    db.execute(
        "UPDATE chat_messages SET text=?1 WHERE id=?2",
        rusqlite::params![legacy.to_string(), message_index],
    )
    .unwrap();
    drop(db);
    let archive = root.path().join("message/.file/bob.dhttp.net/2026-11-01");
    std::fs::create_dir_all(&archive).unwrap();
    let legacy_path = archive.join(legacy["blake3"].as_str().unwrap());
    std::fs::write(&legacy_path, b"old").unwrap();
    let response = invoke(message.clone(), Some(owner.clone()))
        .execute(protocol(json!({"type":"file_read","index":message_index})))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .as_ref(),
        b"old"
    );
    let response = invoke(message.clone(), Some(owner.clone()))
        .execute(protocol(
            json!({"type":"file_save","index":message_index,"directory":"Documents"}),
        ))
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(status, 200, "{bytes:?}");
    std::fs::remove_file(legacy_path).unwrap();
    assert_eq!(
        std::fs::read(root.path().join("file/Documents/historical.txt")).unwrap(),
        b"old"
    );
    reaped(&tasks).await;
}

#[tokio::test]
#[ignore = "run alone with PISHOO_MESSAGE_COMPONENT; real DHTTP source and WASM receiver"]
async fn native_files_explicit_pull_review_range_expiry_and_offline_copy() {
    use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

    use dhttp::resolve::{EndpointAddr, Family, Resolve, ResolveFuture, Source};
    use futures::{FutureExt, StreamExt};
    struct Peer(EndpointAddr);
    impl std::fmt::Debug for Peer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("file test peer")
        }
    }
    impl std::fmt::Display for Peer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("file test peer")
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
        &[("alice", "alice.dhttp.net"), ("bob", "bob.dhttp.net")],
    );
    let previous = std::env::var_os("DHTTP_HOME");
    unsafe {
        std::env::set_var("DHTTP_HOME", root.path());
    }
    let _env = scopeguard::guard(previous, |old| unsafe {
        match old {
            Some(v) => std::env::set_var("DHTTP_HOME", v),
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
    let _socket = scopeguard::guard(socket, |s| {
        qprotocol::Dock::global().remove(&s);
    });
    dhttp::resolve::Resolver::add(Arc::new(Peer(address)));
    let endpoint = dhttp::Endpoint::load("alice").await.unwrap();
    let source = dhttp::Endpoint::load("bob").await.unwrap();
    let local = endpoint.local_authority().unwrap().clone();
    let remote = source.local_authority().unwrap().clone();
    let owner = journal::journal_remote(&local, &local);
    let bob = journal::journal_remote(&remote, &local);
    let hash = |a: &dhttp::LocalAuthority| {
        dhttp_home::certificate::extract_dhttp_subject_key_identifier(a.certificates())
            .unwrap()
            .owner_hash()
            .to_string()
    };
    let remote_hash = hash(&remote);
    let access = Arc::new(
        access_control::AccessService::load_from_db(
            "sqlite::memory:",
            endpoint.name(),
            &access_control::SubjectId::new(hash(&local).as_bytes()).unwrap(),
        )
        .await
        .unwrap(),
    );
    let api = axum::Router::new().route(
        "/std/contact",
        axum::routing::get(move || {
            let hash = remote_hash.clone();
            async move {
                axum::Json(json!({"name":"bob.dhttp.net","status":"active","subject_id":hash}))
            }
        }),
    );
    let data = Arc::new(vec![0x3au8; 4 * 1024 * 1024 + 123]);
    let digest = blake3::hash(&data).to_hex().to_string();
    let tag = format!("\"blake3-{digest}\"");
    let mode = Arc::new(AtomicU8::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let listener=source.listen(dhttp::Scope::Loopback.into(),tower::service_fn({let data=data.clone();let mode=mode.clone();let tag=tag.clone();let calls=calls.clone();move|request:Request<Body>|{let data=data.clone();let mode=mode.clone();let tag=tag.clone();let calls=calls.clone();async move{
        assert_eq!(request.extensions().get::<dhttp::HandshakeSummary>().unwrap().remote.as_ref().unwrap().name(),"alice.dhttp.net");
        calls.fetch_add(1,Ordering::SeqCst);let mode=mode.load(Ordering::SeqCst);
        let response=if request.uri().path()=="/std/acl/status"{
            assert_eq!(request.uri().query(),Some("id=17"));http::Response::builder().header("content-type","application/json").body(axum::body::Body::from(json!({"status":if mode==0{"pending"}else{"allowed"}}).to_string())).unwrap()
        }else if mode==0{http::Response::builder().status(202).header("content-type","application/json").body(axum::body::Body::from(json!({"status":"pending_review","review_id":17,"status_url":"https://untrusted.invalid/admin"}).to_string())).unwrap()
        }else if mode==3{http::Response::builder().status(404).body(axum::body::Body::empty()).unwrap()
        }else{
            assert_eq!(request.uri().path(),"/std/comm/message");assert_eq!(request.method(),http::Method::POST);assert!(request.uri().query().is_none());assert_eq!(request.headers()["if-match"],tag);
            let range=request.headers()["range"].to_str().unwrap().strip_prefix("bytes=").unwrap();let(start,end)=range.split_once('-').unwrap();let start=start.parse::<usize>().unwrap();let end=end.parse::<usize>().unwrap();
            let input: Value = serde_json::from_slice(&request.into_body().collect().await.unwrap().to_bytes()).unwrap();assert_eq!(input,json!({"type":"file_read","id":"a".repeat(32)}));
            let range=format!("bytes {}-{end}/{}",if mode==2{0}else{start},data.len());http::Response::builder().status(206).header("etag",tag).header("content-range",range).header("content-length",end-start+1).body(axum::body::Body::from(data[start..=end].to_vec())).unwrap()
        };Ok::<_,std::convert::Infallible>(response)
    }}})).await.unwrap();
    let _listener = scopeguard::guard(tokio::spawn(listener), |t| t.abort());
    let directory = root.path().join("receiver");
    let lib = load(
        &std::fs::read(std::env::var_os("PISHOO_MESSAGE_COMPONENT").unwrap()).unwrap(),
        &directory,
    );
    let tasks = TaskTracker::new();
    let invoke = |caller| {
        Invocation::new(
            lib.clone(),
            endpoint.clone(),
            &dhttp::HandshakeSummary {
                alpn: None,
                local: Some(local.clone()),
                remote: Some(caller),
            },
            tasks.clone(),
            Outgoing::Granted {
                endpoint: endpoint.clone(),
                access: access.clone(),
                api: api.clone(),
            },
        )
        .unwrap()
    };
    let desc = json!({"version":2,"source_id":"a".repeat(32),"name":"remote.bin","type":"application/octet-stream","size":data.len(),"blake3":digest,"day":"2026-11-06"});
    let doc = json!({"type":"file","from":"bob.dhttp.net","to":"alice.dhttp.net","client_message_id":"incoming","content":desc.to_string()});
    let response = invoke(bob.clone()).execute(protocol(doc)).await.unwrap();
    assert_eq!(response.status(), 201);
    response.into_body().collect().await.unwrap();
    assert!(!directory.join(".file").exists());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let pull = || async {
        let response = invoke(owner.clone())
            .execute(protocol(json!({"type":"download","index":1})))
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice::<Value>(&bytes).unwrap())
    };
    let (status, result) = pull().await;
    assert_eq!(status, 200);
    assert_eq!(result["status"], "review");
    let (status, result) = pull().await;
    assert_eq!(status, 200);
    assert_eq!(result["status"], "review");
    mode.store(1, Ordering::SeqCst);
    let (status, result) = pull().await;
    assert_eq!(status, 200, "{result}");
    assert_eq!(result["status"], "progress");
    assert_eq!(result["offset"], 4 * 1024 * 1024);
    let first_id = result["file_id"].as_str().unwrap().to_owned();
    let response = invoke(bob.clone())
        .execute(protocol(json!({"type":"file_read","id":first_id})))
        .await
        .unwrap();
    assert_eq!(response.status(), 404, "unverified downloads stay private");
    response.into_body().collect().await.unwrap();
    mode.store(2, Ordering::SeqCst);
    let (status, _) = pull().await;
    assert_eq!(status, 502);
    // No guest/native operation is active here; mutate only this isolated test index.
    {
        let db = rusqlite::Connection::open(directory.join(".file/database")).unwrap();
        db.execute("UPDATE files SET created_at=1 WHERE id=?1", [&first_id])
            .unwrap();
    }
    mode.store(1, Ordering::SeqCst);
    let (status, result) = pull().await;
    assert_eq!(status, 200, "{result}");
    assert_ne!(result["file_id"], first_id);
    assert_eq!(result["offset"], 4 * 1024 * 1024);
    let (status, result) = pull().await;
    assert_eq!(status, 200, "{result}");
    assert_eq!(result["status"], "complete");
    let id = result["file_id"].as_str().unwrap().to_owned();
    mode.store(3, Ordering::SeqCst);
    let before = calls.load(Ordering::SeqCst);
    let (status, result) = pull().await;
    assert_eq!(status, 200);
    assert_eq!(result["status"], "complete");
    assert_eq!(calls.load(Ordering::SeqCst), before);
    let response = invoke(owner)
        .execute(protocol(json!({"type":"file_read","id":id})))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(bytes.as_ref(), data.as_slice());
    // The verified copy belongs to Bob's chat directory, so its original
    // sender can read it through the same identity-bound directory rule.
    let response = invoke(bob)
        .execute({
            let mut request = protocol(json!({"type":"file_read","id":id}));
            request
                .headers_mut()
                .insert("range", "bytes=0-2".parse().unwrap());
            request
        })
        .await
        .unwrap();
    assert_eq!(response.status(), 206);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(bytes.as_ref(), &data[..3]);
    reaped(&tasks).await;
}
