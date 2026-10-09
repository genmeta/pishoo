//! Run explicitly after building examples/note/build.sh: these execute the real guest.
use serde_json::{Value, json};

use super::*;

fn note_lib(directory: &Path) -> Arc<Lib> {
    let path = std::env::var_os("PISHOO_NOTE_WASM")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/note/dist/lib.wasm")
        });
    Arc::new(
        Lib::load(
            Arc::new(WasmRuntime::new().unwrap()),
            "note".into(),
            &std::fs::read(path).expect("build Note WASM first"),
            directory,
        )
        .unwrap(),
    )
}

async fn call(
    lib: &Arc<Lib>,
    tasks: &TaskTracker,
    method: &str,
    path: &str,
    input: Value,
    version: Option<&str>,
) -> (u16, http::HeaderMap, Bytes) {
    let mut request = Request::builder()
        .method(method)
        .uri(format!("https://alice.dhttp.net{path}"));
    if let Some(version) = version {
        request = request.header("if-match", format!("\"{version}\""));
    }
    let bytes = if input.is_null() {
        Vec::new()
    } else {
        serde_json::to_vec(&input).unwrap()
    };
    if !bytes.is_empty() {
        request = request.header("content-type", "application/json");
    }
    let request = request
        .body(
            Full::new(Bytes::from(bytes))
                .map_err(|never| match never {})
                .boxed_unsync(),
        )
        .unwrap();
    let response = tokio::time::timeout(
        Duration::from_secs(15),
        invoke(lib.clone(), tasks).await.execute(request),
    )
    .await
    .unwrap()
    .unwrap();
    let (parts, body) = response.into_parts();
    let bytes = tokio::time::timeout(Duration::from_secs(15), body.collect())
        .await
        .unwrap()
        .unwrap()
        .to_bytes();
    (parts.status.as_u16(), parts.headers, bytes)
}

#[tokio::test]
#[ignore = "build examples/note/build.sh before running the real Note guest"]
async fn note_component_page_crud_sqlite_persistence_input_and_identity_isolation() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    let lib = note_lib(&data);
    let tasks = TaskTracker::new();
    let id = "e0b8713acdb84942b878b88e2c343264";
    let path = format!("/note?id={id}");
    let (status, headers, page) = call(&lib, &tasks, "GET", "/", Value::Null, None).await;
    assert_eq!(status, 200);
    assert_eq!(headers["content-type"], "text/html; charset=utf-8");
    let page = std::str::from_utf8(&page).unwrap();
    assert!(page.contains("我的便签"));
    assert!(page.contains("fetch(path,"));
    assert!(page.contains("api('notes?q='"));
    assert!(!page.contains("/std/api/"));
    assert!(!page.contains("/api/note"));
    let (status, _, bytes) = call(&lib, &tasks, "GET", "/notes", Value::Null, None).await;
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap(),
        json!({"notes":[]})
    );
    let note = json!({"id":id,"title":"旅行计划","body":"中文\n☕ <script>alert(1)</script>"});
    let (status, headers, bytes) = call(&lib, &tasks, "POST", "/notes", note.clone(), None).await;
    assert_eq!(status, 201, "{}", String::from_utf8_lossy(&bytes));
    assert_eq!(headers["etag"], "\"1\"");
    let created: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(created["body"], note["body"]);
    assert_eq!(
        call(&lib, &tasks, "POST", "/notes", note, None).await.0,
        409
    );
    reaped(&tasks).await;
    drop(lib);
    // A freshly loaded component, with fresh runtime and filesystem grants,
    // must read data left by the previous invocation.
    let lib = note_lib(&data);
    let tasks = TaskTracker::new();
    assert_eq!(
        serde_json::from_slice::<Value>(
            &call(&lib, &tasks, "GET", &path, Value::Null, None).await.2
        )
        .unwrap(),
        created
    );
    let (status, _, bytes) = call(
        &lib,
        &tasks,
        "GET",
        "/notes?q=%E6%97%85%E8%A1%8C",
        Value::Null,
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap()["notes"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let update = json!({"title":"更新","body":"新的内容"});
    assert_eq!(
        call(&lib, &tasks, "PUT", &path, update.clone(), None)
            .await
            .0,
        428
    );
    let (status, headers, _) = call(&lib, &tasks, "PUT", &path, update.clone(), Some("1")).await;
    assert_eq!(status, 200);
    assert_eq!(headers["etag"], "\"2\"");
    assert_eq!(
        call(&lib, &tasks, "PUT", &path, update, Some("1")).await.0,
        412
    );
    assert_eq!(
        call(&lib, &tasks, "DELETE", &path, Value::Null, Some("1"))
            .await
            .0,
        412
    );
    assert_eq!(
        call(&lib, &tasks, "DELETE", &path, Value::Null, Some("2"))
            .await
            .0,
        204
    );
    assert_eq!(
        call(&lib, &tasks, "GET", &path, Value::Null, None).await.0,
        404
    );
    assert_eq!(
        call(
            &lib,
            &tasks,
            "PUT",
            &path,
            json!({"title":"复活","body":"不允许"}),
            Some("2")
        )
        .await
        .0,
        404
    );
    assert_eq!(
        call(&lib, &tasks, "GET", "/note?id=../ssl", Value::Null, None)
            .await
            .0,
        400
    );
    assert_eq!(
        call(
            &lib,
            &tasks,
            "POST",
            "/notes",
            json!({"id":"a".repeat(32),"title":"","body":" "}),
            None
        )
        .await
        .0,
        400
    );
    assert_eq!(
        call(
            &lib,
            &tasks,
            "POST",
            "/notes",
            json!({"id":"a".repeat(32),"title":"超长","body":"x".repeat(65537)}),
            None
        )
        .await
        .0,
        413
    );
    let sibling = note_lib(&root.path().join("sibling"));
    assert_eq!(
        call(&sibling, &tasks, "GET", "/notes", Value::Null, None)
            .await
            .2,
        Bytes::from_static(b"{\"notes\":[]}")
    );
    reaped(&tasks).await;
    let db = rusqlite::Connection::open(data.join("note.db")).unwrap();
    assert_eq!(
        db.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    drop(db);
    std::fs::write(data.join("note.db"), b"broken database").unwrap();
    let tasks = TaskTracker::new();
    assert_eq!(
        call(&lib, &tasks, "GET", &path, Value::Null, None).await.0,
        500
    );
    reaped(&tasks).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "build examples/note/build.sh before running the real Note guest"]
async fn note_independent_invocations_atomically_compete_for_one_revision() {
    let data = tempfile::tempdir().unwrap();
    let lib = note_lib(data.path());
    let tasks = TaskTracker::new();
    let id = "b".repeat(32);
    let path = format!("/note?id={id}");
    assert_eq!(
        call(
            &lib,
            &tasks,
            "POST",
            "/notes",
            json!({"id":id,"title":"开始","body":"版本一"}),
            None
        )
        .await
        .0,
        201
    );
    let attempts = (0..8).map(|index| {
        let lib = lib.clone();
        let tasks = tasks.clone();
        let path = path.clone();
        async move {
            call(
                &lib,
                &tasks,
                "PUT",
                &path,
                json!({"title":format!("设备 {index}"),"body":"x".repeat(32000)}),
                Some("1"),
            )
            .await
        }
    });
    let results = futures::future::join_all(attempts).await;
    assert_eq!(
        results
            .iter()
            .filter(|(status, _, _)| *status == 200)
            .count(),
        1,
        "{results:?}"
    );
    assert_eq!(
        results
            .iter()
            .filter(|(status, _, _)| *status == 412)
            .count(),
        7
    );
    let note: Value =
        serde_json::from_slice(&call(&lib, &tasks, "GET", &path, Value::Null, None).await.2)
            .unwrap();
    assert_eq!(note["version"], "2");
    assert_eq!(note["body"].as_str().unwrap().len(), 32000);
    reaped(&tasks).await;
    let db = rusqlite::Connection::open(data.path().join("note.db")).unwrap();
    assert_eq!(
        db.query_row("SELECT version FROM notes WHERE id=?1", [&id], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        db.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    reaped(&tasks).await;
}

#[tokio::test]
#[ignore = "build examples/note/build.sh before running the real Note guest"]
async fn note_sandbox_mounts_only_its_named_database_directory() {
    let home = tempfile::tempdir().unwrap();
    let profile =
        dhttp_home::identity::IdentityProfile::try_from(home.path().join("alice")).unwrap();
    let bytes =
        std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/note/dist/lib.wasm"))
            .unwrap();
    for id in ["note", "other"] {
        let dir = profile.join(format!("lib/{id}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("lib.wasm"), &bytes).unwrap();
    }
    std::fs::create_dir_all(profile.join("db")).unwrap();
    std::fs::write(profile.join("db/config.db"), b"host-private").unwrap();
    let mut sandbox = Sandbox::new(Arc::new(WasmRuntime::new().unwrap()));
    sandbox.load_libs(&profile).unwrap();
    let lib = sandbox.libs.get("note").unwrap();
    assert_eq!(
        call(
            lib,
            &sandbox.tasks,
            "POST",
            "/notes",
            json!({"id":"c".repeat(32),"title":"隔离","body":"只在 Note 中"}),
            None
        )
        .await
        .0,
        201
    );
    let other = sandbox.libs.get("other").unwrap();
    assert_eq!(
        call(other, &sandbox.tasks, "GET", "/notes", Value::Null, None)
            .await
            .2,
        Bytes::from_static(b"{\"notes\":[]}")
    );
    assert!(profile.join("db/note/note.db").is_file());
    assert!(profile.join("db/other/note.db").is_file());
    assert!(!profile.join("db/note.db").exists());
    assert!(!profile.join("lib/note/data").exists());
    assert_eq!(
        std::fs::read(profile.join("db/config.db")).unwrap(),
        b"host-private"
    );
    use wasmtime_wasi::{
        filesystem::WasiFilesystemCtxView,
        p2::bindings::filesystem::{
            preopens::Host,
            types::{DescriptorFlags, HostDescriptor, OpenFlags, PathFlags},
        },
    };
    let mut table = ResourceTable::new();
    let mut filesystem = lib.filesystem.clone();
    let mut view = WasiFilesystemCtxView {
        ctx: &mut filesystem,
        table: &mut table,
    };
    let dirs = Host::get_directories(&mut view).unwrap();
    assert_eq!(dirs.len(), 1);
    assert_eq!(dirs[0].1, "/db");
    let descriptor = dirs[0].0.rep();
    for path in ["../config.db", "../other/note.db"] {
        assert!(
            view.open_at(
                wasmtime::component::Resource::new_borrow(descriptor),
                PathFlags::empty(),
                path.into(),
                OpenFlags::empty(),
                DescriptorFlags::READ
            )
            .await
            .is_err()
        );
    }
    sandbox.close();
    sandbox.wait().await.unwrap();
}

/// Temporary loopback UI fixture; every API request still executes the real guest.
#[tokio::test]
#[ignore = "manual browser QA: serves the real Note guest on 127.0.0.1:18743 for five minutes"]
async fn note_browser_preview() {
    let data = tempfile::tempdir().unwrap();
    let lib = note_lib(data.path());
    let mut sandbox = Sandbox::new(lib.runtime.clone());
    sandbox.libs.insert("note".into(), lib);
    let local = authority("alice.dhttp.net");
    let app = sandbox
        .api_router(crate::test_identity::endpoint("alice.dhttp.net"))
        .layer(axum::middleware::from_fn(
            move |mut request: Request<axum::body::Body>, next: axum::middleware::Next| {
                let local = local.clone();
                async move {
                    *request.uri_mut() = format!("https://alice.dhttp.net{}", request.uri())
                        .parse()
                        .unwrap();
                    request.extensions_mut().insert(dhttp::HandshakeSummary {
                        alpn: None,
                        local: Some(local),
                        remote: None,
                    });
                    next.run(request).await
                }
            },
        ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:18743")
        .await
        .unwrap();
    eprintln!("Note real WASM browser fixture: http://127.0.0.1:18743/std/api/note/");
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
    tokio::select! {
        _ = serving => (),
        _ = tokio::time::sleep(Duration::from_secs(300)) => (),
    }
    sandbox.close();
    sandbox.wait().await.unwrap();
}
