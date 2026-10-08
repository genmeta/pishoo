//! Exercise the actual binary with isolated homes and fake service managers.
use std::{
    path::Path,
    process::{Command, Output},
};

use serde_json::Value;

fn run(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pishoo"))
        .env("DHTTP_HOME", home)
        .args(args)
        .output()
        .unwrap()
}
fn success(home: &Path, args: &[&str]) -> Output {
    let output = run(home, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}
fn initialize(home: &Path, name: &str) {
    let dir = home.join(name);
    std::fs::create_dir_all(dir.join("db")).unwrap();
    std::fs::create_dir(dir.join("lib")).unwrap();
    let db = rusqlite::Connection::open(dir.join("db/config.db")).unwrap();
    db.execute_batch("CREATE TABLE settings(listen INTEGER NOT NULL); INSERT INTO settings VALUES(0); CREATE TABLE proxy_locations(location TEXT PRIMARY KEY, proxy_pass TEXT NOT NULL); PRAGMA user_version=1;").unwrap();
}
fn component() -> Vec<u8> {
    let mut bytes = include_bytes!("fixtures/wasi-http-read-request-then-respond.wasm").to_vec();
    let doc = br#"{"openapi":"3.1.0","info":{"title":"Note","version":"1"},"paths":{"/upload":{"post":{}}}}"#;
    let mut section = vec![14];
    section.extend_from_slice(b"pishoo:openapi");
    section.extend_from_slice(doc);
    let mut n = section.len();
    bytes.push(0);
    loop {
        let b = (n & 127) as u8;
        n >>= 7;
        bytes.push(b | if n > 0 { 128 } else { 0 });
        if n == 0 {
            break;
        }
    }
    bytes.extend(section);
    bytes
}
#[test]
fn local_commands_use_default_or_explicit_identity_without_starting_a_server() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path();
    initialize(home, "alice.smith");
    initialize(home, "bob");
    std::fs::write(
        home.join("settings.toml"),
        "[default]\nname='alice.smith.dhttp.net'\n",
    )
    .unwrap();
    let before = std::fs::read(home.join("settings.toml")).unwrap();
    assert!(String::from_utf8_lossy(&success(home, &["listen"]).stdout).contains("off (0)"));
    success(home, &["--id", "alice.smith", "listen", "internal"]);
    success(home, &["listen", "-i", "alice.smith", "both"]);
    assert!(String::from_utf8_lossy(&success(home, &["listen"]).stdout).contains("both (3)"));
    assert!(
        String::from_utf8_lossy(&success(home, &["listen", "-i", "bob"]).stdout)
            .contains("off (0)")
    );
    success(home, &["proxy", "/test", "127.0.0.1:8080"]);
    let rule: Value = serde_json::from_slice(&success(home, &["proxy", "/test"]).stdout).unwrap();
    assert_eq!(rule["proxy_pass"], "http://127.0.0.1:8080");
    success(home, &["proxy", "= /exact", "http://127.0.0.1:8081/"]);
    success(home, &["proxy", "rm", "/test"]);
    success(home, &["proxy", "remove", "/test"]);
    let rules: Value = serde_json::from_slice(&success(home, &["proxy", "ls"]).stdout).unwrap();
    assert_eq!(rules.as_array().unwrap().len(), 1);
    let path = home.join("proxies.json");
    std::fs::write(&path, b"[]").unwrap();
    success(home, &["proxy", "replace", path.to_str().unwrap()]);
    success(home, &["proxy", "clear"]);
    let input = home.join("note.wasm");
    std::fs::write(&input, component()).unwrap();
    let output = success(home, &["lib", "check", input.to_str().unwrap()]);
    let checked: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(checked.get("id").is_none());
    success(
        home,
        &[
            "lib",
            "-i",
            "alice.smith",
            "install",
            "note",
            input.to_str().unwrap(),
        ],
    );
    assert!(!home.join("alice.smith/db/note").exists());
    // A disk resource cannot masquerade as a running catalog on connection failure.
    let loaded = run(home, &["lib", "--loaded"]);
    assert_eq!(loaded.status.code(), Some(1));
    assert!(loaded.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&loaded.stderr).contains("Catalog:    installed (disk)"));
    let info: Value =
        serde_json::from_slice(&success(home, &["lib", "info", "note"]).stdout).unwrap();
    assert_eq!(info["endpoints"][0]["path"], "/api/note/upload");
    let libs: Value = serde_json::from_slice(&success(home, &["lib"]).stdout).unwrap();
    assert_eq!(libs[0]["id"], "note");
    std::fs::create_dir(home.join("alice.smith/db/note")).unwrap();
    std::fs::write(home.join("alice.smith/db/note/keep.db"), b"keep").unwrap();
    success(home, &["lib", "rm", "note"]);
    success(home, &["lib", "remove", "note"]);
    assert_eq!(
        std::fs::read(home.join("alice.smith/db/note/keep.db")).unwrap(),
        b"keep"
    );
    assert_eq!(std::fs::read(home.join("settings.toml")).unwrap(), before);
    assert!(
        !home.join("logs/error.log").exists(),
        "commands must not initialize daemon logging"
    );
}
#[test]
fn invalid_commands_and_defaults_fail_without_initializing_or_selecting_another_identity() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path();
    initialize(home, "alice");
    for args in [
        vec!["listen"],
        vec!["listen", "wrong"],
        vec!["--id", "alice"],
        vec!["start", "--id", "alice"],
        vec!["lib", "check", "-i", "alice", "a.wasm"],
        vec!["proxy", "remove"],
        vec!["lib", "--loaded", "extra"],
    ] {
        assert_eq!(run(home, &args).status.code(), Some(2), "{args:?}");
    }
    for contents in [
        "broken = [",
        "[default]\nname='absent'",
        "[default]\nname=12",
        "[something]\nx=1",
    ] {
        std::fs::write(home.join("settings.toml"), contents).unwrap();
        let output = run(home, &["lib"]);
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains("--id"));
    }
    let input = home.join("bad.wasm");
    std::fs::write(&input, b"bad").unwrap();
    assert_eq!(
        run(home, &["lib", "check", input.to_str().unwrap()])
            .status
            .code(),
        Some(2)
    );
    assert_eq!(
        run(home, &["lib", "check", "/nonexistent-pishoo-test.wasm"])
            .status
            .code(),
        Some(1)
    );
    std::fs::remove_file(home.join("alice/db/config.db")).unwrap();
    assert_eq!(
        run(home, &["listen", "-i", "alice", "both"]).status.code(),
        Some(1)
    );
    assert!(!home.join("alice/db/config.db").exists());
    success(home, &["--help"]);
    success(home, &["--version"]);
    assert!(!home.join("logs").exists());
}
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn service_commands_use_the_installed_manager_without_shell_or_escalation() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let executable = root.path().join(if cfg!(target_os = "macos") {
        "brew"
    } else {
        "systemctl"
    });
    let log = root.path().join("calls");
    std::fs::write(&executable, "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$PISHOO_TEST_LOG\"\nif [ \"$PISHOO_TEST_FAIL\" = 1 ]; then exit 1; fi\nif [ \"$1\" = show ]; then printf 'loaded\\n'; fi\nexit 0\n").unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    for operation in ["start", "stop", "restart", "status"] {
        let output = Command::new(env!("CARGO_BIN_EXE_pishoo"))
            .env("PATH", root.path())
            .env("DHTTP_HOME", root.path())
            .env("PISHOO_TEST_LOG", &log)
            .arg(operation)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let calls = std::fs::read_to_string(&log).unwrap();
    for operation in ["start", "stop", "restart"] {
        assert!(calls.contains(&if cfg!(target_os = "macos") {
            format!("services {operation} pishoo\n")
        } else {
            format!("{operation} pishoo.service\n")
        }));
    }
    assert!(!calls.contains("sudo"));
    let output = Command::new(env!("CARGO_BIN_EXE_pishoo"))
        .env("PATH", root.path())
        .env("DHTTP_HOME", root.path())
        .env("PISHOO_TEST_LOG", &log)
        .env("PISHOO_TEST_FAIL", "1")
        .arg("restart")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(!root.path().join("logs").exists());
}
