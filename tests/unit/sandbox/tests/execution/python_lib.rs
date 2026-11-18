use std::io::Read;

use serde_json::{Value, json};

use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "run alone with PYTHON_LIB_PACKAGE and OpenSSL 3; uses real TLS identities and a Jade-built WAZ"]
async fn jade_python_lib_executes_cpython_with_scoped_files_and_limits() {
    let package = std::env::var_os("PYTHON_LIB_PACKAGE").expect("set PYTHON_LIB_PACKAGE");
    let mut archive = zip::ZipArchive::new(std::fs::File::open(package).unwrap()).unwrap();
    let mut bytes = Vec::new();
    archive
        .by_name("backend.wasm")
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let (local, owner, bob) = journal::journal_credentials(root.path()).await;
    let endpoint = dhttp::Endpoint::load_from(root.path().join("alice"))
        .await
        .unwrap();
    let runtime = Arc::new(WasmRuntime::new().unwrap());
    let data = root.path().join("python-data");
    let lib = Arc::new(
        Lib::load(
            runtime,
            "dev.genmeta.tech.dhttp.net/python".into(),
            &bytes,
            &data,
        )
        .unwrap(),
    );
    assert_eq!(lib.commands.len(), 1);
    let tasks = TaskTracker::new();
    let call = |value: Value, remote: dhttp::RemoteAuthority| {
        let lib = lib.clone();
        let local = local.clone();
        let endpoint = endpoint.clone();
        let tasks = tasks.clone();
        async move {
            let invocation = Invocation::new(
                lib,
                endpoint,
                &dhttp::HandshakeSummary {
                    alpn: None,
                    local: Some(local),
                    remote: Some(remote),
                },
                tasks,
                Outgoing::Denied,
            )
            .unwrap();
            let request = Request::post("https://alice.dhttp.net/run/python")
                .header("content-type", "application/json")
                .body(
                    Full::new(Bytes::from(serde_json::to_vec(&value).unwrap()))
                        .map_err(|never| match never {})
                        .boxed_unsync(),
                )
                .unwrap();
            let response = invocation.execute(request).await.unwrap();
            let status = response.status();
            assert_eq!(response.headers()["cache-control"], "no-store");
            let body = response.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
            serde_json::from_slice::<Value>(&body).unwrap()
        }
    };
    let result=call(json!({"workspace":"test","code":"import json,sys,os\nprint(json.dumps({'sum':sum(json.load(sys.stdin)),'argv':sys.argv,'cwd':os.getcwd()}))\nos.write(2,b'raw-error\\n')","stdin":"[1,2,3]","args":["中文 空格"]}),owner.clone()).await;
    assert_eq!(result["exit_code"], 0, "{result}");
    assert_eq!(result["stderr"], "raw-error\n");
    let stdout: Value = serde_json::from_str(result["stdout"].as_str().unwrap()).unwrap();
    assert_eq!(
        stdout,
        json!({"sum":6,"argv":["<run-python>","中文 空格"],"cwd":"/work"})
    );
    for (code, exit) in [
        ("import sys\nsys.exit(7)", 7),
        ("raise ValueError('test')", 1),
        ("if True print('bad')", 1),
    ] {
        let result = call(json!({"workspace":"test","code":code}), owner.clone()).await;
        assert_eq!(result["exit_code"], exit, "{result}");
        assert_eq!(result["termination_reason"], "exited");
    }
    let result=call(json!({"workspace":"test","code":"from pathlib import Path\nPath('value.txt').write_text('private')\nPath('helper.py').write_text('value=42\\n')\nprint('saved')"}),owner.clone()).await;
    assert_eq!(result["stdout"], "saved\n", "{result}");
    let result=call(json!({"workspace":"test","code":"from pathlib import Path\nimport helper\nassert helper.value==42\nprint(Path('value.txt').read_text())"}),owner.clone()).await;
    assert_eq!(result["stdout"], "private\n", "{result}");
    let result=call(json!({"workspace":"test","code":"from pathlib import Path\nprint(Path('value.txt').exists())\nfor path in ['/db','/work/../value.txt','/runtime/new-file']:\n try:Path(path).write_text('bad');print('UNEXPECTED')\n except OSError:pass"}),bob.clone()).await;
    assert_eq!(result["stdout"], "False\n", "{result}");
    for code in [
        "import os\nos.write(1,b'x'*(2*1024*1024))",
        "import os\nos.write(1,b'a'*(700*1024))\nos.write(2,b'b'*(700*1024))",
    ] {
        let result = call(json!({"workspace":"test","code":code}), owner.clone()).await;
        assert_eq!(result["termination_reason"], "output_limit", "{result}");
        assert_eq!(
            result["stdout"].as_str().unwrap().len() + result["stderr"].as_str().unwrap().len(),
            1024 * 1024
        );
    }
    for code in ["while True: pass", "import time\ntime.sleep(60)"] {
        let started = std::time::Instant::now();
        let result = call(
            json!({"workspace":"test","code":code,"timeout_ms":500}),
            owner.clone(),
        )
        .await;
        assert_eq!(result["termination_reason"], "timeout", "{result}");
        assert!(started.elapsed() < Duration::from_secs(3));
    }
    let result = call(
        json!({"workspace":"test","code":"while True: pass"}),
        owner.clone(),
    )
    .await;
    assert_eq!(result["termination_reason"], "fuel_exhausted", "{result}");
    let mut pending = Box::pin(call(
        json!({"workspace":"test","code":"import time\ntime.sleep(60)"}),
        owner,
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(300), &mut pending)
            .await
            .is_err()
    );
    drop(pending);
    reaped(&tasks).await;
    assert!(tasks.is_empty());
}
