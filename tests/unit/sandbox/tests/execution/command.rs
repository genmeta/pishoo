//! Optional real Command Lib coverage using its locally built component.
use serde_json::{Value, json};

use super::*;

async fn identities() -> &'static (
    dhttp::LocalAuthority,
    dhttp::RemoteAuthority,
    dhttp::RemoteAuthority,
    dhttp::Endpoint,
) {
    static IDENTITIES: tokio::sync::OnceCell<(
        dhttp::LocalAuthority,
        dhttp::RemoteAuthority,
        dhttp::RemoteAuthority,
        dhttp::Endpoint,
    )> = tokio::sync::OnceCell::const_new();
    IDENTITIES
        .get_or_init(|| async {
            let root = tempfile::tempdir().unwrap();
            let (local, owner, bob) = journal::journal_credentials(root.path()).await;
            let endpoint = dhttp::Endpoint::load_from(root.path().join("alice"))
                .await
                .unwrap();
            (local, owner, bob, endpoint)
        })
        .await
}

fn command_lib(directory: &Path) -> Arc<Lib> {
    let path = std::env::var_os("PISHOO_COMMAND_COMPONENT")
        .expect("set PISHOO_COMMAND_COMPONENT to the prepared Command backend");
    Arc::new(
        Lib::load(
            Arc::new(WasmRuntime::new().unwrap()),
            "dev.genmeta.tech.dhttp.net/command".into(),
            &std::fs::read(path).unwrap(),
            directory,
        )
        .unwrap(),
    )
}

async fn call(
    lib: Arc<Lib>,
    tasks: &TaskTracker,
    caller: Option<&str>,
    input: Value,
) -> (u16, Value) {
    let (local, owner, bob, endpoint) = identities().await;
    let invocation = Invocation::new(
        lib,
        endpoint.clone(),
        &dhttp::HandshakeSummary {
            alpn: None,
            local: Some(local.clone()),
            remote: caller.map(|name| {
                if name == "alice.dhttp.net" {
                    owner.clone()
                } else {
                    bob.clone()
                }
            }),
        },
        tasks.clone(),
        Outgoing::Denied,
    )
    .unwrap();
    let body = Full::new(Bytes::from(serde_json::to_vec(&input).unwrap()))
        .map_err(|never| match never {})
        .boxed_unsync();
    let request = Request::post("https://alice.dhttp.net/run/command")
        .header("content-type", "application/json")
        .body(body)
        .unwrap();
    let response = invocation.execute(request).await.unwrap();
    assert_eq!(response.headers()["cache-control"], "no-store");
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
#[ignore = "requires locally built Command HTTP Lib"]
async fn command_http_shell_semantics_and_persistent_files() {
    let root = tempfile::tempdir().unwrap();
    let lib = command_lib(&root.path().join("command"));
    let tasks = TaskTracker::new();
    for (script, output, code) in [
        ("printf '%s\\n' 'hello world'", "hello world\n", 0),
        (
            "n=0; for x in a b c; do n=$((n+1)); done; printf '%s' \"$n\"",
            "3",
            0,
        ),
        ("f() { printf '<%s>' \"$1\"; }; f 'a b'", "<a b>", 0),
        (
            "printf 'a\\nb\\n' | while IFS= read -r line; do printf '<%s>\\n' \"$line\"; done",
            "<a>\n<b>\n",
            0,
        ),
        ("printf 'stored\\n' > 'space file'", "", 0),
        (
            "read -r value < 'space file'; printf '%s' \"$value\"",
            "stored",
            0,
        ),
        ("printf err >&2; exit 7", "", 7),
        ("false || printf recovered", "recovered", 0),
        (
            "v=outer; printf '%s' \"$(v=inner; printf '%s' \"$v\")\"; printf '%s' \"$v\"",
            "innerouter",
            0,
        ),
        ("printf '%s' *.file", "*.file", 0),
        ("printf side-effect > syntax-marker; if", "", 2),
        (
            "if test -e syntax-marker; then printf leaked; else printf clean; fi",
            "clean",
            0,
        ),
        (
            "for file in *; do printf '<%s>' \"$file\"; done",
            "<space file>",
            0,
        ),
    ] {
        let (status, result) = call(
            lib.clone(),
            &tasks,
            Some("alice.dhttp.net"),
            json!({"workspace":"smoke","command":script}),
        )
        .await;
        assert_eq!(status, 200, "{script}: {result}");
        assert_eq!(result["termination_reason"], "exited", "{script}: {result}");
        assert_eq!(result["stdout"], output, "{script}: {result}");
        assert_eq!(result["exit_code"], code, "{script}: {result}");
    }
    let (_, result) = call(lib, &tasks, Some("alice.dhttp.net"),
        json!({"workspace":"smoke","command":"read -r v; printf '%s:%s' \"$TARGET\" \"$v\"", "stdin":"input\n", "env":{"TARGET":"value"}})).await;
    assert_eq!(result["stdout"], "value:input", "{result}");
    reaped(&tasks).await;
}

#[tokio::test]
#[ignore = "requires locally built Command HTTP Lib"]
async fn command_http_identity_validation_and_directory_boundaries() {
    let root = tempfile::tempdir().unwrap();
    let lib = command_lib(&root.path().join("command"));
    let tasks = TaskTracker::new();
    let (status, _) = call(
        lib.clone(),
        &tasks,
        None,
        json!({"workspace":"w","command":"true"}),
    )
    .await;
    assert_eq!(status, 403);
    assert!(!root.path().join("command/workspaces").exists());
    for input in [
        json!({"workspace":"../other","command":"true"}),
        json!({"workspace":"w","command":"true","cwd":"/db"}),
        json!({"workspace":"w","command":"true","unknown":true}),
    ] {
        assert_eq!(
            call(lib.clone(), &tasks, Some("alice.dhttp.net"), input)
                .await
                .0,
            400
        );
    }
    let (_, write) = call(
        lib.clone(),
        &tasks,
        Some("alice.dhttp.net"),
        json!({"workspace":"w","command":"printf secret > private"}),
    )
    .await;
    assert_eq!(write["exit_code"], 0, "{write}");
    let (_, other) = call(lib.clone(), &tasks, Some("bob.dhttp.net"),
        json!({"workspace":"w","command":"if test -e private; then printf leaked; else printf isolated; fi"})).await;
    assert_eq!(other["stdout"], "isolated", "{other}");
    let (_, denied) = call(
        lib,
        &tasks,
        Some("alice.dhttp.net"),
        json!({"workspace":"w","command":"printf escaped > /db/escape"}),
    )
    .await;
    assert_ne!(denied["exit_code"], 0, "{denied}");
    assert!(!root.path().join("command/escape").exists());
    reaped(&tasks).await;
}

#[tokio::test]
#[ignore = "requires locally built Command HTTP Lib"]
async fn command_http_timeout_output_limit_and_drop_reap() {
    let root = tempfile::tempdir().unwrap();
    let lib = command_lib(&root.path().join("command"));
    let tasks = TaskTracker::new();
    let (_, result) = call(
        lib.clone(),
        &tasks,
        Some("alice.dhttp.net"),
        json!({"workspace":"w","command":"while :; do :; done", "timeout_ms":100}),
    )
    .await;
    assert_eq!(result["termination_reason"], "timeout", "{result}");
    let (_, result) = call(
        lib.clone(),
        &tasks,
        Some("alice.dhttp.net"),
        json!({"workspace":"w","command":"while :; do printf '%01024d' 0; done"}),
    )
    .await;
    assert_eq!(result["termination_reason"], "output_limit", "{result}");
    assert_eq!(result["truncated"], true);
    assert!(result["stdout"].as_str().unwrap().len() <= 1024 * 1024);
    let pending = call(
        lib,
        &tasks,
        Some("alice.dhttp.net"),
        json!({"workspace":"w","command":"while :; do :; done", "timeout_ms":60_000}),
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), pending)
            .await
            .is_err()
    );
    reaped(&tasks).await;
}

#[tokio::test]
#[ignore = "requires locally built Command HTTP Lib"]
async fn command_http_standard_dispatch_rechecks_post_acl() {
    use access_control::{Effect, Grantee};
    use tower::ServiceExt;

    let root = tempfile::tempdir().unwrap();
    let lib = command_lib(&root.path().join("command"));
    let (local, _, bob, endpoint) = identities().await;
    let ski = dhttp_home::certificate::extract_dhttp_subject_key_identifier(local.certificates())
        .unwrap();
    let access = Arc::new(
        access_control::AccessService::load_from_db(
            "sqlite::memory:",
            local.name(),
            &access_control::SubjectId::new(ski.owner_hash().as_str().as_bytes()).unwrap(),
        )
        .await
        .unwrap(),
    );
    let mut sandbox = Sandbox::new(lib.runtime.clone());
    sandbox
        .libs
        .insert("dev.genmeta.tech.dhttp.net/command".into(), lib);
    let tasks = sandbox.tasks.clone();
    let api = Sandbox::api_router(
        Arc::new(RwLock::new(sandbox)),
        endpoint.clone(),
        access.clone(),
        axum::Router::new(),
    )
    .layer(axum::middleware::from_fn_with_state(
        access.clone(),
        crate::routes::authorize,
    ));
    for (effect, expected) in [(Effect::Allow, 200), (Effect::Deny, 403)] {
        access
            .set_policy(
                access_control::Method::Specified(http::Method::POST),
                "/std/run/command",
                effect,
                Grantee::All,
            )
            .await
            .unwrap();
        let mut request = Request::post("https://alice.dhttp.net/std/run/command")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                json!({"workspace":"router","command":"printf routed"}).to_string(),
            ))
            .unwrap();
        request.extensions_mut().insert(dhttp::HandshakeSummary {
            alpn: None,
            local: Some(local.clone()),
            remote: Some(bob.clone()),
        });
        let response = api.clone().oneshot(request).await.unwrap();
        let status = response.status().as_u16();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(status, expected, "{}", String::from_utf8_lossy(&bytes));
        if status == 200 {
            let result: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(result["stdout"], "routed", "{result}");
        }
    }
    reaped(&tasks).await;
}
