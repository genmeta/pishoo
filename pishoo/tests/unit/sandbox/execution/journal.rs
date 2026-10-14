//! Identity provenance and persistence checks against the actual Journal component.
use super::*;

#[path = "../../../support/network_credentials.rs"]
mod network_credentials;

async fn journal_credentials(
    root: &Path,
) -> (
    dhttp::LocalAuthority,
    dhttp::RemoteAuthority,
    dhttp::RemoteAuthority,
) {
    network_credentials::generate(
        root,
        &[("alice", "alice.dhttp.net"), ("bob", "bob.dhttp.net")],
    );
    qtls::RootCerts::set([qtls::CertificateDer::from(
        std::fs::read(root.join("ca.der")).unwrap(),
    )])
    .unwrap();
    let mut authorities = Vec::new();
    for name in ["alice", "bob"] {
        let profile = dhttp_home::identity::IdentityProfile::try_from(root.join(name)).unwrap();
        authorities.push(
            dhttp::LocalAuthority::new(
                &qtls::default_provider(),
                Arc::from(format!("{name}.dhttp.net")),
                profile.load_certs().await.unwrap(),
                profile.load_key().await.unwrap(),
                profile.load_ocsp().await.unwrap(),
            )
            .unwrap(),
        );
    }
    let owner = journal_remote(&authorities[0], &authorities[0]);
    let bob = journal_remote(&authorities[1], &authorities[0]);
    (authorities.remove(0), owner, bob)
}

// RemoteAuthority has no public constructor: obtain it through the real,
// certificate- and OCSP-verified TLS handshake, without changing qtls.
fn journal_remote(
    client: &dhttp::LocalAuthority,
    server: &dhttp::LocalAuthority,
) -> dhttp::RemoteAuthority {
    let provider = Arc::new(qtls::default_provider());
    let client_tls = qtls::TlsClient::new(qtls::ClientTlsConfig {
        provider: provider.clone(),
        alpn: vec![b"h3".to_vec()],
        authority: Some(client.clone()),
        resumption: qtls::ClientResumptionConfig::Disabled,
        limits: qtls::TlsLimits::default(),
    })
    .unwrap();
    let server_tls = qtls::TlsServer::new(qtls::ServerTlsConfig {
        provider,
        alpn: vec![b"h3".to_vec()],
        authority: server.clone(),
        resumption: qtls::ServerResumptionConfig::Disabled,
        limits: qtls::TlsLimits::default(),
    })
    .unwrap();
    let mut client = client_tls
        .start(qtls::ClientStart {
            server_name: qtls::ServerName::try_from(server.name().to_owned()).unwrap(),
            quic_version: qtls::QuicVersion::V1,
            local_transport_parameters: Bytes::new(),
        })
        .unwrap();
    let mut server = server_tls
        .start(qtls::QuicVersion::V1, Bytes::new())
        .unwrap();
    for _ in 0..16 {
        while let Some(event) = client.next_event() {
            if let qtls::TlsEvent::WriteCrypto { epoch, bytes } = event {
                server.receive_crypto(epoch, &bytes).unwrap();
            }
        }
        while let Some(event) = server.next_event() {
            match event {
                qtls::TlsEvent::WriteCrypto { epoch, bytes } => {
                    client.receive_crypto(epoch, &bytes).unwrap()
                }
                qtls::TlsEvent::HandshakeComplete(summary) => return summary.remote.unwrap(),
                _ => (),
            }
        }
    }
    panic!("verified TLS handshake did not complete");
}
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

fn journal_authority(name: &str, owner: char) -> dhttp::LocalAuthority {
    let mut params = rcgen::CertificateParams::new(vec![name.to_owned()]).unwrap();
    let ski = format!("0:0:{}", owner.to_string().repeat(64));
    let mut der = vec![4, ski.len() as u8];
    der.extend_from_slice(ski.as_bytes());
    params
        .custom_extensions
        .push(rcgen::CustomExtension::from_oid_content(
            &[2, 5, 29, 14],
            der,
        ));
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = params.self_signed(&key).unwrap();
    dhttp::LocalAuthority::new(
        &qtls::default_provider(),
        Arc::from(name),
        vec![cert.der().clone()],
        qtls::PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
        vec![1],
    )
    .unwrap()
}

#[tokio::test]
#[ignore = "run alone: generates credentials and sets process-wide TLS roots"]
async fn request_identity_is_derived_from_handshake_and_owner_requires_name_and_key() {
    use identity::pishoo::identity::request_identity::Host;
    let root = tempfile::tempdir().unwrap();
    let (local, owner, bob) = journal_credentials(root.path()).await;
    let mut state = StoreData {
        table: ResourceTable::new(),
        wasi: wasmtime_wasi::WasiCtx::builder().build(),
        http: wasmtime_wasi_http::WasiHttpCtx::new(),
        memory: StoreLimits::default(),
        outgoing: Outgoing::Denied,
        local: local.clone(),
        remote: None,
    };
    use crate::sandbox::host::identity_v2::pishoo::identity::request_identity::Host as HostV2;
    let verified_local = HostV2::local(&mut state).await.unwrap();
    assert_eq!(verified_local.0, "alice.dhttp.net");
    assert_eq!(HostV2::caller(&mut state).await.unwrap(), None);
    assert_eq!(Host::local_name(&mut state).await, "alice.dhttp.net");
    assert_eq!(Host::caller_name(&mut state).await, None);
    assert!(!Host::caller_is_owner(&mut state).await);
    state.remote = Some(owner.clone());
    assert!(Host::caller_is_owner(&mut state).await);
    assert_eq!(
        HostV2::caller(&mut state).await.unwrap(),
        Some(verified_local.clone())
    );
    state.local = journal_authority("alice.dhttp.net", 'b');
    assert!(!Host::caller_is_owner(&mut state).await);
    assert_ne!(
        HostV2::caller(&mut state).await.unwrap(),
        Some(HostV2::local(&mut state).await.unwrap())
    );
    state.local = local;
    state.remote = Some(bob);
    assert_eq!(
        Host::caller_name(&mut state).await,
        Some("bob.dhttp.net".into())
    );
    assert!(!Host::caller_is_owner(&mut state).await);
    assert_eq!(
        HostV2::caller(&mut state).await.unwrap().unwrap().0,
        "bob.dhttp.net"
    );
    assert_ne!(
        HostV2::caller(&mut state).await.unwrap(),
        Some(verified_local)
    );
    let invalid = rcgen::generate_simple_self_signed(vec!["alice.dhttp.net".into()]).unwrap();
    state.local = dhttp::LocalAuthority::new(
        &qtls::default_provider(),
        Arc::from("alice.dhttp.net"),
        vec![invalid.cert.der().clone()],
        qtls::PrivateKeyDer::try_from(invalid.signing_key.serialize_der()).unwrap(),
        vec![1],
    )
    .unwrap();
    state.remote = Some(owner);
    assert!(!Host::caller_is_owner(&mut state).await);
    assert!(HostV2::local(&mut state).await.is_err());
}

fn journal_lib(directory: &Path) -> Arc<Lib> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/journal");
    let bytes = std::fs::read(root.join("lib.wasm")).expect("build Journal backend first");
    let document: Value =
        serde_json::from_slice(&std::fs::read(root.join("openapi.json")).unwrap()).unwrap();
    let package = package_fixture(
        &document,
        &bytes,
        webc::v3::write::Directory::from_path(root.join("public")).unwrap(),
        &publisher("alice.smith.dhttp.net", 'a'),
    );
    Arc::new(
        Lib::load(
            Arc::new(WasmRuntime::new().unwrap()),
            "journal".into(),
            &package,
            directory,
        )
        .unwrap(),
    )
}

async fn journal_call(
    lib: &Arc<Lib>,
    tasks: &TaskTracker,
    local: &dhttp::LocalAuthority,
    remote: Option<&dhttp::RemoteAuthority>,
    method: &str,
    path: &str,
    document: Value,
    version: Option<&str>,
    extra: &[(&str, &str)],
) -> (u16, http::HeaderMap, Bytes) {
    let mut builder = Request::builder()
        .method(method)
        .uri(format!("https://alice.dhttp.net{path}"));
    if !document.is_null() {
        builder = builder.header("content-type", "application/json");
    }
    if method != "GET" && method != "HEAD" {
        builder = builder.header("x-journal-write", "1");
    }
    if let Some(version) = version {
        builder = builder.header("if-match", format!("\"{version}\""));
    }
    let mut request = builder
        .body(
            Full::new(if document.is_null() {
                Bytes::new()
            } else {
                Bytes::from(serde_json::to_vec(&document).unwrap())
            })
            .map_err(|never| match never {})
            .boxed_unsync(),
        )
        .unwrap();
    for (name, value) in extra {
        request.headers_mut().append(
            http::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
    }
    let invocation = Invocation::new(
        lib.clone(),
        crate::test_identity::endpoint("alice.dhttp.net"),
        &dhttp::HandshakeSummary {
            alpn: None,
            local: Some(local.clone()),
            remote: remote.cloned(),
        },
        tasks.clone(),
        Outgoing::Denied,
    )
    .unwrap();
    let response = tokio::time::timeout(Duration::from_secs(20), invocation.execute(request))
        .await
        .unwrap()
        .unwrap();
    let (parts, body) = response.into_parts();
    let bytes = tokio::time::timeout(Duration::from_secs(20), body.collect())
        .await
        .unwrap()
        .unwrap()
        .to_bytes();
    (parts.status.as_u16(), parts.headers, bytes)
}

#[tokio::test]
#[ignore = "build examples/journal/build.sh before running the real Journal guest"]
async fn journal_component_identity_crud_comments_photos_and_persistence() {
    let data = tempfile::tempdir().unwrap();
    let lib = journal_lib(data.path());
    let tasks = TaskTracker::new();
    let credentials = tempfile::tempdir().unwrap();
    let (local, owner, bob) = journal_credentials(credentials.path()).await;
    let spoof = [
        ("pishoo-local-name", "mallory.dhttp.net"),
        ("pishoo-caller-name", "alice.dhttp.net"),
        ("pishoo-caller-is-owner", "true"),
    ];
    assert_eq!(
        journal_call(
            &lib,
            &tasks,
            &local,
            None,
            "GET",
            "/context",
            Value::Null,
            None,
            &spoof
        )
        .await
        .0,
        403
    );
    assert!(!data.path().join("journal.db").exists());
    let (status, _, context) = journal_call(
        &lib,
        &tasks,
        &local,
        Some(&bob),
        "GET",
        "/context",
        Value::Null,
        None,
        &spoof,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_slice::<Value>(&context).unwrap(),
        json!({"owner_name":"alice.dhttp.net","caller_name":"bob.dhttp.net","is_owner":false})
    );
    // The backend owns APIs only; the root page is served from the WEBC volume.
    assert_eq!(
        journal_call(
            &lib,
            &tasks,
            &local,
            Some(&owner),
            "GET",
            "/",
            Value::Null,
            None,
            &[]
        )
        .await
        .0,
        404
    );
    let id = "a".repeat(32);
    let doc = json!({"id":id,"title":"周末","body":"中文与 <script> 内容按文字保存"});
    assert_eq!(
        journal_call(
            &lib,
            &tasks,
            &local,
            Some(&bob),
            "POST",
            "/entries",
            doc.clone(),
            None,
            &spoof
        )
        .await
        .0,
        403
    );
    let replacement = journal_authority("alice.dhttp.net", 'c');
    assert_eq!(
        journal_call(
            &lib,
            &tasks,
            &replacement,
            Some(&owner),
            "POST",
            "/entries",
            doc.clone(),
            None,
            &[]
        )
        .await
        .0,
        403
    );
    assert_eq!(
        journal_call(
            &lib,
            &tasks,
            &local,
            Some(&owner),
            "POST",
            "/entries",
            doc.clone(),
            None,
            &[("origin", "https://evil.example")]
        )
        .await
        .0,
        403
    );
    assert_eq!(
        journal_call(
            &lib,
            &tasks,
            &local,
            Some(&owner),
            "POST",
            "/entries",
            doc.clone(),
            None,
            &[("x-journal-write", "0")]
        )
        .await
        .0,
        403
    );
    let (status, headers, created) = journal_call(
        &lib,
        &tasks,
        &local,
        Some(&owner),
        "POST",
        "/entries",
        doc.clone(),
        None,
        &[],
    )
    .await;
    assert_eq!(status, 201);
    assert_eq!(headers["etag"], "\"1\"");
    assert_eq!(
        serde_json::from_slice::<Value>(&created).unwrap()["body"],
        doc["body"]
    );
    let comment_id = "b".repeat(32);
    let comment = json!({"id":comment_id,"entry_id":id,"body":"好看的手记"});
    let mut fake = comment.clone();
    fake["author"] = json!("alice.dhttp.net");
    assert_eq!(
        journal_call(
            &lib,
            &tasks,
            &local,
            Some(&bob),
            "POST",
            "/comments",
            fake,
            None,
            &[]
        )
        .await
        .0,
        400
    );
    let (status, _, created) = journal_call(
        &lib,
        &tasks,
        &local,
        Some(&bob),
        "POST",
        "/comments",
        comment.clone(),
        None,
        &spoof,
    )
    .await;
    assert_eq!(status, 201);
    assert_eq!(
        serde_json::from_slice::<Value>(&created).unwrap()["author"],
        "bob.dhttp.net"
    );
    assert_eq!(
        journal_call(
            &lib,
            &tasks,
            &local,
            Some(&bob),
            "POST",
            "/comments",
            comment,
            None,
            &[]
        )
        .await
        .0,
        409
    );
    let reply_id = "c".repeat(32);
    let reply = json!({"id":reply_id,"entry_id":id,"parent_id":comment_id,"body":"谢谢！"});
    assert_eq!(
        journal_call(
            &lib,
            &tasks,
            &local,
            Some(&owner),
            "POST",
            "/comments",
            reply,
            None,
            &[]
        )
        .await
        .0,
        201
    );
    assert_eq!(journal_call(&lib,&tasks,&local,Some(&owner),"POST","/comments",json!({"id":"d".repeat(32),"entry_id":id,"parent_id":reply_id,"body":"不能回复到第二层"}),None,&[]).await.0,400);
    let update = json!({"title":"更新","body":"已经修改"});
    let path = format!("/entry?id={id}");
    assert_eq!(
        journal_call(
            &lib,
            &tasks,
            &local,
            Some(&owner),
            "PUT",
            &path,
            update.clone(),
            None,
            &[]
        )
        .await
        .0,
        428
    );
    assert_eq!(
        journal_call(
            &lib,
            &tasks,
            &local,
            Some(&owner),
            "PUT",
            &path,
            update.clone(),
            Some("1"),
            &[]
        )
        .await
        .0,
        200
    );
    assert_eq!(
        journal_call(
            &lib,
            &tasks,
            &local,
            Some(&owner),
            "PUT",
            &path,
            update,
            Some("1"),
            &[]
        )
        .await
        .0,
        412
    );
    let image = include_bytes!("../../../fixtures/journal-photo.jpg");
    let mut with_metadata = vec![0xff, 0xd8, 0xff, 0xe1, 0, 18];
    with_metadata.extend_from_slice(b"Exif\0\0GPS_SECRET");
    with_metadata.extend_from_slice(&image[2..]);
    let photo = format!("data:image/jpeg;base64,{}", STANDARD.encode(&with_metadata));
    assert_eq!(
        journal_call(
            &lib,
            &tasks,
            &local,
            Some(&owner),
            "PUT",
            &path,
            json!({"title":"照片","body":"有照片","photo":photo}),
            Some("2"),
            &[]
        )
        .await
        .0,
        200
    );
    let (status, headers, image) = journal_call(
        &lib,
        &tasks,
        &local,
        Some(&bob),
        "GET",
        &format!("/photo?id={id}"),
        Value::Null,
        None,
        &[],
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(headers["content-type"], "image/jpeg");
    assert!(!image.windows(10).any(|b| b == b"GPS_SECRET"));
    assert!(imagesize::blob_size(&image).is_ok());
    assert_eq!(
        journal_call(
            &lib,
            &tasks,
            &local,
            Some(&owner),
            "PUT",
            &path,
            json!({"title":"照片","body":"无效照片","photo":"data:image/svg+xml;base64,PHN2Zz4="}),
            Some("3"),
            &[]
        )
        .await
        .0,
        400
    );
    let reloaded = journal_lib(data.path());
    let (status, _, bytes) = journal_call(
        &reloaded,
        &tasks,
        &local,
        Some(&bob),
        "GET",
        &path,
        Value::Null,
        None,
        &[],
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap()["has_photo"],
        true
    );
    let isolated = tempfile::tempdir().unwrap();
    let other = journal_lib(isolated.path());
    assert_eq!(
        journal_call(
            &other,
            &tasks,
            &local,
            Some(&bob),
            "GET",
            &path,
            Value::Null,
            None,
            &[]
        )
        .await
        .0,
        404
    );
    let comment_path = format!("/comment?id={comment_id}");
    assert_eq!(
        journal_call(
            &lib,
            &tasks,
            &local,
            Some(&bob),
            "DELETE",
            &comment_path,
            Value::Null,
            None,
            &[]
        )
        .await
        .0,
        403
    );
    assert_eq!(
        journal_call(
            &lib,
            &tasks,
            &local,
            Some(&owner),
            "DELETE",
            &comment_path,
            Value::Null,
            None,
            &[]
        )
        .await
        .0,
        204
    );
    let (_, _, comments) = journal_call(
        &lib,
        &tasks,
        &local,
        Some(&bob),
        "GET",
        &format!("/comments?entry_id={id}"),
        Value::Null,
        None,
        &[],
    )
    .await;
    let comments: Value = serde_json::from_slice(&comments).unwrap();
    assert!(
        comments["comments"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == comment_id
                && c["author"].is_null()
                && c["body"].is_null()
                && c["deleted"] == true)
    );
    assert_eq!(
        journal_call(
            &lib,
            &tasks,
            &local,
            Some(&owner),
            "DELETE",
            &path,
            Value::Null,
            Some("3"),
            &[]
        )
        .await
        .0,
        204
    );
    assert_eq!(
        journal_call(
            &lib,
            &tasks,
            &local,
            Some(&bob),
            "GET",
            &path,
            Value::Null,
            None,
            &[]
        )
        .await
        .0,
        404
    );
    assert_eq!(
        journal_call(
            &lib,
            &tasks,
            &local,
            Some(&bob),
            "GET",
            &format!("/photo?id={id}"),
            Value::Null,
            None,
            &[]
        )
        .await
        .0,
        404
    );
    assert_eq!(
        journal_call(
            &lib,
            &tasks,
            &local,
            Some(&owner),
            "POST",
            "/entries",
            doc,
            None,
            &[]
        )
        .await
        .0,
        409
    );
    let db = rusqlite::Connection::open(data.path().join("journal.db")).unwrap();
    assert_eq!(
        db.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    reaped(&tasks).await;
}

#[tokio::test]
#[ignore = "build examples/journal/build.sh before running the real Journal guest"]
async fn journal_component_origin_shorthand_preserves_cross_site_rejection() {
    let data = tempfile::tempdir().unwrap();
    let lib = journal_lib(data.path());
    let tasks = TaskTracker::new();
    let credentials = tempfile::tempdir().unwrap();
    let (local, owner, bob) = journal_credentials(credentials.path()).await;
    let id = "f".repeat(32);
    assert_eq!(
        journal_call(
            &lib,
            &tasks,
            &local,
            Some(&owner),
            "POST",
            "/entries",
            json!({"id":id,"title":"Origin 回归","body":"同源简写"}),
            None,
            &[("origin", "https://alice~")]
        )
        .await
        .0,
        201
    );
    for (index, origin) in [
        "https://alice~",
        "https://alice.dhttp.net",
        "https://ALICE~:443",
        "https://~",
    ]
    .into_iter()
    .enumerate()
    {
        let (status, _, body) = journal_call(
            &lib,
            &tasks,
            &local,
            Some(&owner),
            "POST",
            "/comments",
            json!({"id":format!("{:032x}",index+100),"entry_id":id,"body":"同源留言"}),
            None,
            &[("origin", origin), ("sec-fetch-site", "same-origin")],
        )
        .await;
        assert_eq!(
            status,
            201,
            "allowed Origin {origin}: {}",
            String::from_utf8_lossy(&body)
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap()["author"],
            "alice.dhttp.net"
        );
    }
    assert_eq!(
        journal_call(
            &lib,
            &tasks,
            &local,
            Some(&bob),
            "POST",
            "/comments",
            json!({"id":"e".repeat(32),"entry_id":id,"body":"获准具名读者也可同源留言"}),
            None,
            &[("origin", "https://alice~")]
        )
        .await
        .0,
        201
    );
    for origin in [
        "https://bob~",
        "https://alice~.evil.example",
        "http://alice~",
        "https://alice~:8443",
        "https://alice~:99999",
        "https://user@alice~",
        "https://alice~/",
        "https://alice~?x=1",
        "https://alice~#fragment",
        "null",
    ] {
        let (status, _, body) = journal_call(
            &lib,
            &tasks,
            &local,
            Some(&owner),
            "POST",
            "/comments",
            json!({"id":"d".repeat(32),"entry_id":id,"body":"不得写入"}),
            None,
            &[("origin", origin)],
        )
        .await;
        assert_eq!(
            status,
            403,
            "rejected Origin {origin}: {}",
            String::from_utf8_lossy(&body)
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap()["error"],
            "cross_origin_denied"
        );
    }
    assert_eq!(
        journal_call(
            &lib,
            &tasks,
            &local,
            Some(&bob),
            "POST",
            "/comments",
            json!({"id":"c".repeat(32),"entry_id":id,"body":"裸简写属于 Bob，不能当作 Alice"}),
            None,
            &[("origin", "https://~")]
        )
        .await
        .0,
        403
    );
    assert_eq!(
        journal_call(
            &lib,
            &tasks,
            &local,
            Some(&owner),
            "POST",
            "/comments",
            json!({"id":"b".repeat(32),"entry_id":id,"body":"跨站标记仍拒绝"}),
            None,
            &[
                ("origin", "https://alice~"),
                ("sec-fetch-site", "cross-site")
            ]
        )
        .await
        .0,
        403
    );
    assert_eq!(
        journal_call(
            &lib,
            &tasks,
            &local,
            Some(&owner),
            "POST",
            "/comments",
            json!({"id":"a".repeat(32),"entry_id":id,"body":"重复 Origin 拒绝"}),
            None,
            &[("origin", "https://alice~"), ("origin", "https://alice~")]
        )
        .await
        .0,
        403
    );
    reaped(&tasks).await;
}

#[tokio::test]
#[ignore = "build examples/journal/build.sh before running the real Journal guest"]
async fn journal_component_concurrent_updates_only_accept_one_version() {
    let data = tempfile::tempdir().unwrap();
    let lib = journal_lib(data.path());
    let tasks = TaskTracker::new();
    let credentials = tempfile::tempdir().unwrap();
    let (local, owner, _) = journal_credentials(credentials.path()).await;
    let id = "e".repeat(32);
    let path = format!("/entry?id={id}");
    assert_eq!(
        journal_call(
            &lib,
            &tasks,
            &local,
            Some(&owner),
            "POST",
            "/entries",
            json!({"id":id,"title":"并发","body":"原始版本"}),
            None,
            &[]
        )
        .await
        .0,
        201
    );
    let (a, b) = tokio::join!(
        journal_call(
            &lib,
            &tasks,
            &local,
            Some(&owner),
            "PUT",
            &path,
            json!({"title":"设备一","body":"一"}),
            Some("1"),
            &[]
        ),
        journal_call(
            &lib,
            &tasks,
            &local,
            Some(&owner),
            "PUT",
            &path,
            json!({"title":"设备二","body":"二"}),
            Some("1"),
            &[]
        )
    );
    let mut statuses = [a.0, b.0];
    statuses.sort();
    assert_eq!(statuses, [200, 412]);
    reaped(&tasks).await;
}

#[tokio::test]
#[ignore = "manual browser QA: isolated real WASM on 127.0.0.1:18747; synthetic test owner, no production data"]
async fn journal_browser_preview() {
    let credentials = tempfile::tempdir().unwrap();
    let (local, owner, _) = journal_credentials(credentials.path()).await;
    let data = tempfile::tempdir().unwrap();
    let lib = journal_lib(data.path());
    let mut sandbox = Sandbox::new(lib.runtime.clone());
    sandbox.libs.insert("journal".into(), lib);
    let app = api_router(&sandbox, crate::test_identity::endpoint("alice.dhttp.net")).layer(
        axum::middleware::from_fn(
            move |mut request: Request<axum::body::Body>, next: axum::middleware::Next| {
                let local = local.clone();
                let remote = owner.clone();
                async move {
                    *request.uri_mut() = format!("http://127.0.0.1:18747{}", request.uri())
                        .parse()
                        .unwrap();
                    request.extensions_mut().insert(dhttp::HandshakeSummary {
                        alpn: None,
                        local: Some(local),
                        remote: Some(remote),
                    });
                    next.run(request).await
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:18747")
        .await
        .unwrap();
    eprintln!(
        "Journal real WASM browser fixture: http://127.0.0.1:18747/api/journal/ (test owner, temporary data)"
    );
    use tower::ServiceExt;
    let serving = async {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let app = app.clone();
            tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        hyper_util::rt::TokioIo::new(stream),
                        hyper::service::service_fn(move |request| app.clone().oneshot(request)),
                    )
                    .await;
            });
        }
    };
    tokio::select! {_ = serving => (),_ = tokio::time::sleep(Duration::from_secs(300))=>()}
    sandbox.close();
    sandbox.wait().await.unwrap();
}

#[tokio::test]
#[ignore = "build Journal backend first; run alone with process-wide TLS roots"]
async fn journal_frontend_is_served_from_package_with_daccess_for_every_file() {
    use access_control::{Effect, Grantee, SubjectId};
    use http::{Method, StatusCode};
    use tower::ServiceExt;

    let credentials = tempfile::tempdir().unwrap();
    let (local, _, bob) = journal_credentials(credentials.path()).await;
    let ski = dhttp_home::certificate::extract_dhttp_subject_key_identifier(local.certificates())
        .unwrap();
    let access = Arc::new(
        access_control::AccessService::load_from_db(
            "sqlite::memory:",
            local.name(),
            &SubjectId::new(ski.owner_hash().as_str().as_bytes()).unwrap(),
        )
        .await
        .unwrap(),
    );
    let data = tempfile::tempdir().unwrap();
    let lib = journal_lib(data.path());
    let mut sandbox = Sandbox::new(lib.runtime.clone());
    let id = "alice.smith.dhttp.net/journal";
    sandbox.libs.insert(id.into(), lib.clone());
    let app = api_router(&sandbox, crate::test_identity::endpoint("alice.dhttp.net")).layer(
        axum::middleware::from_fn_with_state(access.clone(), crate::routes::authorize),
    );
    let public = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/journal/public");
    for (suffix, file, media) in [
        ("/", "index.html", "text/html"),
        ("/assets/app.js", "assets/app.js", "text/javascript"),
        ("/assets/app.css", "assets/app.css", "text/css"),
    ] {
        let path = format!("/api/{}{suffix}", id.rsplit('/').next().unwrap());
        for grantee in [Grantee::Anony, Grantee::Named] {
            access
                .set_policy(
                    access_control::Method::Unspecified,
                    &path,
                    Effect::Deny,
                    grantee,
                )
                .await
                .unwrap();
        }
        for remote in [None, Some(bob.clone())] {
            let mut request = Request::builder()
                .uri(&path)
                .body(axum::body::Body::empty())
                .unwrap();
            request.extensions_mut().insert(dhttp::HandshakeSummary {
                alpn: None,
                local: Some(local.clone()),
                remote,
            });
            assert_eq!(
                app.clone().oneshot(request).await.unwrap().status(),
                StatusCode::FORBIDDEN
            );
        }
        access
            .set_policy(
                access_control::Method::Unspecified,
                &path,
                Effect::Allow,
                Grantee::Named,
            )
            .await
            .unwrap();
        let expected = std::fs::read(public.join(file)).unwrap();
        for method in [Method::GET, Method::HEAD] {
            let mut request = Request::builder()
                .method(method.clone())
                .uri(&path)
                .body(axum::body::Body::empty())
                .unwrap();
            request.extensions_mut().insert(dhttp::HandshakeSummary {
                alpn: None,
                local: Some(local.clone()),
                remote: Some(bob.clone()),
            });
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()["content-type"], media);
            assert_eq!(
                response.headers()["content-length"],
                expected.len().to_string()
            );
            assert_eq!(response.headers()["cache-control"], "no-store");
            assert_eq!(response.headers()["referrer-policy"], "no-referrer");
            assert_eq!(
                response.headers()["cross-origin-resource-policy"],
                "same-origin"
            );
            assert_eq!(response.headers()["x-frame-options"], "DENY");
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(
                bytes.as_ref(),
                if method == Method::HEAD {
                    b""
                } else {
                    expected.as_slice()
                }
            );
        }
    }
    assert!(
        sandbox.tasks.is_empty(),
        "static files must not invoke WASM"
    );
    assert!(!data.path().join("journal.db").exists());
    let index = std::fs::read_to_string(public.join("index.html")).unwrap();
    assert!(index.contains("./assets/app.js") && index.contains("./assets/app.css"));
    assert!(!index.contains("<script>") && !index.contains("<style>"));
}
