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
    assert_eq!(result["termination_reason"], "output_limit");
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

#[tokio::test]
#[ignore = "requires locally built Command HTTP Lib"]
async fn command_http_file_tools_and_streaming_pipelines() {
    let root = tempfile::tempdir().unwrap();
    let lib = command_lib(&root.path().join("command"));
    let tasks = TaskTracker::new();
    let cases = [
        (
            "copy and move",
            r#"mkdir -p source/sub; printf 'alpha\nbeta\n' > source/sub/a.txt; cp -r source copy; mv copy/sub/a.txt copy/sub/b.txt; cat copy/sub/b.txt"#,
            "alpha\nbeta\n",
        ),
        (
            "text pipeline",
            r#"printf 'b\na\nb\nc\n' | sort | uniq | tr a-z A-Z"#,
            "A\nB\nC\n",
        ),
        (
            "early consumer",
            "yes hello | head -n 3",
            "hello\nhello\nhello\n",
        ),
        ("large producer", "seq 1 100000 | head -n 3", "1\n2\n3\n"),
        (
            "heredoc and script",
            "cat > calc.sh <<'SCRIPT'\nn=$1\nprintf '%s\\n' \"$((n * 2))\"\nSCRIPT\nsh calc.sh 21",
            "42\n",
        ),
        ("grep", r#"grep -n '^a' source/sub/a.txt"#, "1:alpha\n"),
        (
            "edit in place",
            r#"sed -i 's/beta/gamma/' source/sub/a.txt; cat source/sub/a.txt"#,
            "alpha\ngamma\n",
        ),
        (
            "JSON",
            r#"printf '%s' '{"items":[{"value":3},{"value":4}]}' | jq -r '.items | map(.value) | add'"#,
            "7\n",
        ),
        ("base64", "printf abc | base64 | base64 -d", "abc"),
        (
            "checksum",
            "printf abc | sha256sum",
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  -\n",
        ),
        (
            "metadata",
            r#"printf abc > sized; stat -c '%n %s %F' sized"#,
            "sized 3 regular file\n",
        ),
        (
            "same file",
            r#"printf keep > safe; cp safe safe; code=$?; printf '%s:' "$code"; cat safe"#,
            "1:keep",
        ),
        (
            "hard links",
            r#"ln safe alias; test safe -ef alias; printf '%s:' "$?"; cp safe alias; code=$?; printf '%s:' "$code"; cat safe"#,
            "0:1:keep",
        ),
        (
            "symbolic links",
            "ln -s safe symbolic; cat symbolic; rm symbolic; cat safe",
            "keepkeep",
        ),
        (
            "recursive links",
            "mkdir tree; ln -s .. tree/up; cp -r tree saved; test -L saved/up; printf '%s' \"$?\"",
            "0",
        ),
        (
            "recursive self copy",
            "cp -r tree tree/inside; code=$?; printf '%s:' \"$code\"; test ! -e tree/inside; printf '%s' \"$?\"",
            "1:0",
        ),
        (
            "redirected errors",
            "cat /db/secret 2>denied.txt; code=$?; test -s denied.txt; printf '%s:%s' \"$code\" \"$?\"",
            "1:0",
        ),
        ("sandbox identity", "whoami; uname -s", "wasi\nWASI\n"),
    ];
    for (name, script, expected) in cases {
        let (status, result) = call(
            lib.clone(),
            &tasks,
            Some("alice.dhttp.net"),
            json!({"workspace":"toolkit","command":script}),
        )
        .await;
        assert_eq!(status, 200, "{name}: {result}");
        assert_eq!(result["termination_reason"], "exited", "{name}: {result}");
        assert_eq!(result["exit_code"], 0, "{name}: {result}");
        assert_eq!(result["stdout"], expected, "{name}: {result}");
    }
    for script in ["not-a-tool", "ls --not-supported", "cat /db/secret"] {
        let (_, result) = call(
            lib.clone(),
            &tasks,
            Some("alice.dhttp.net"),
            json!({"workspace":"toolkit","command":script}),
        )
        .await;
        assert_ne!(result["exit_code"], 0, "{script}: {result}");
        assert!(
            !result["stderr"].as_str().unwrap().is_empty(),
            "{script}: {result}"
        );
    }
    let (_, result) = call(
        lib,
        &tasks,
        Some("alice.dhttp.net"),
        json!({"workspace":"toolkit","command":"date +%Y-%m-%d"}),
    )
    .await;
    assert_eq!(result["exit_code"], 0, "{result}");
    let date = result["stdout"].as_str().unwrap().trim();
    assert_eq!(date.len(), 10);
    assert_eq!(&date[4..5], "-");
    assert_eq!(&date[7..8], "-");
    reaped(&tasks).await;
}

#[tokio::test]
#[ignore = "requires locally built Command HTTP Lib"]
async fn command_http_streaming_and_script_regressions() {
    let root = tempfile::tempdir().unwrap();
    let lib = command_lib(&root.path().join("command"));
    let tasks = TaskTracker::new();
    for (name, script, expected, code) in [
        ("quiet broken pipe", "seq 1 100000 | head -n 1", "1\n", 0),
        (
            "quiet cat pipe",
            "seq 1 10000 > numbers; cat numbers | head -n 1",
            "1\n",
            0,
        ),
        ("pipefail", "set -o pipefail; false | cat", "", 1),
        (
            "null device",
            "cat /missing 2>/dev/null; cat /dev/null; printf done >/dev/null; printf ok",
            "ok",
            0,
        ),
        (
            "find and xargs",
            "mkdir -p inputs; touch inputs/a.txt inputs/b.txt; find inputs -type f -name '*.txt' | sort | xargs basename -a",
            "a.txt\nb.txt\n",
            0,
        ),
        (
            "file ordering",
            "n=0; while test \"$n\" -lt 100; do printf '%s' \"$n\" > value; read actual < value; test \"$actual\" = \"$n\" || exit 5; n=$((n+1)); done; cat value",
            "99",
            0,
        ),
        ("stdin untouched", "printf ok", "ok", 0),
        (
            "script source",
            "printf 'x=loaded\\n' > source.sh; . ./source.sh; printf '%s' \"$x\"",
            "loaded",
            0,
        ),
        (
            "numeric format",
            "printf '%05d %s\\n' 7 hello",
            "00007 hello\n",
            0,
        ),
        ("format reuse", "printf '%s\\n' a b c", "a\nb\nc\n", 0),
        (
            "UTF-8 read and EOF",
            r#"printf '你好\n世界' > unicode; while IFS= read -r line || test -n "$line"; do printf '<%s>\n' "$line"; done < unicode"#,
            "<你好>\n<世界>\n",
            0,
        ),
        (
            "sh command string",
            r#"sh -c 'printf "%s:%s" "$0" "$1"' task hello"#,
            "task:hello",
            0,
        ),
        (
            "supplementary discovery",
            "command -v whoami; command -v sh; type whoami",
            "/bin/whoami\nsh\nwhoami is a shell builtin\n",
            0,
        ),
        (
            "numeric diagnostic redirection",
            r#"printf '%d' invalid 2>number.err; code=$?; test -s number.err; printf ':%s:%s' "$code" "$?""#,
            "0:1:0",
            0,
        ),
        (
            "command inventory",
            r#"help | grep -q 'mkdir'; printf '%s' "$?""#,
            "0",
            0,
        ),
        ("background cleanup", "sleep 30 & printf done", "done", 0),
    ] {
        let (status, result) = call(
            lib.clone(),
            &tasks,
            Some("alice.dhttp.net"),
            json!({"workspace":"regressions", "command":script, "stdin":"x".repeat(64 * 1024)}),
        )
        .await;
        assert_eq!(status, 200, "{name}: {result}");
        assert_eq!(result["termination_reason"], "exited", "{name}: {result}");
        assert_eq!(result["stdout"], expected, "{name}: {result}");
        assert_eq!(result["stderr"], "", "{name}: {result}");
        assert_eq!(result["exit_code"], code, "{name}: {result}");
    }
    reaped(&tasks).await;
}
