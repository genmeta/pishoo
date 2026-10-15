//! Exercise the actual binary with isolated homes and fake service managers.
use std::{
    path::Path,
    process::{Command, Output},
};

use serde_json::Value;

fn run(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pishoo"))
        .env("HOME", home.parent().unwrap())
        .env("DHTTP_HOME", "/nonexistent-ignored-pishoo-home")
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
    std::fs::create_dir(dir.join("libs")).unwrap();
    let db = rusqlite::Connection::open(dir.join("db/config.db")).unwrap();
    db.execute_batch("CREATE TABLE settings(listen INTEGER NOT NULL, enabled INTEGER NOT NULL DEFAULT 1); INSERT INTO settings(listen) VALUES(0); CREATE TABLE proxy_locations(location TEXT PRIMARY KEY, proxy_pass TEXT NOT NULL); PRAGMA user_version=1;").unwrap();
}
fn component() -> Vec<u8> {
    use std::{collections::BTreeMap, sync::Arc};

    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use sha2::{Digest, Sha256};
    use webc::v3::{
        SignatureAlgorithm, Timestamps,
        write::{Directory, FileEntry, Writer},
    };
    let name = "alice.dhttp.net";
    let mut params = rcgen::CertificateParams::new(vec![name.into()]).unwrap();
    let ski = format!("1:0:{}", "a".repeat(64));
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
    let authority = dhttp::LocalAuthority::new(
        &qtls::default_provider(),
        Arc::from(name),
        vec![cert.der().clone()],
        qtls::PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
        vec![1],
    )
    .unwrap();
    let bytes = include_bytes!("fixtures/wasi-http-read-request-then-respond.wasm");
    let mut doc = serde_json::json!({"openapi":"3.1.0","info":{"title":"Note","description":"CLI fixture","version":"1.0.0","license":{"name":"MIT"}},"x-pishoo":{"kind":"lib","publisher":name,"name":"note","host":{}},"paths":{"/upload":{"post":{"operationId":"upload","summary":"Upload"}}}});
    doc["x-pishoo"] = serde_json::json!({});
    let manifest:webc::metadata::Manifest=serde_json::from_value(serde_json::json!({"package":{
        "wapm":{"name":"alice.dhttp.net/note","version":"1.0.0","description":"CLI fixture","license":"MIT"},
        "pishoo":{"kind":"lib","title":"Note","dependencies":[],"host":{},"changelog":"CLI test; data unchanged"},
        "fs":[{"volume_name":"metadata","mount_path":"/metadata"}]},"atoms":{"backend":{"kind":"https://webc.org/kind/wasm","signature":format!("sha256:{}",STANDARD.encode(Sha256::digest(bytes)))}}})).unwrap();
    let atoms = || {
        BTreeMap::from([(
            "backend".parse().unwrap(),
            FileEntry::borrowed(bytes, Timestamps::default()),
        )])
    };
    let metadata = || {
        Directory::new(
            BTreeMap::from([(
                "openapi.json".parse().unwrap(),
                serde_json::to_vec(&doc).unwrap().into(),
            )]),
            Timestamps::default(),
        )
    };
    let unsigned = Writer::default()
        .write_manifest(&manifest)
        .unwrap()
        .write_atoms(atoms())
        .unwrap()
        .with_volume("metadata", metadata())
        .unwrap()
        .finish(SignatureAlgorithm::None)
        .unwrap();
    let reader = webc::v3::read::OwnedReader::parse(unsigned.clone()).unwrap();
    let index = reader.index();
    let mut hash = Sha256::new();
    hash.update(b"\0webc003");
    for entry in std::iter::once(&index.manifest)
        .chain(std::iter::once(&index.atoms))
        .chain(index.volumes.values())
    {
        hash.update(&unsigned[entry.span.start..entry.span.end()]);
    }
    let mut input = b"pishoo-webc-signature-v1\0".to_vec();
    input.extend_from_slice(&hash.finalize());
    let envelope = serde_json::json!({"version":1,"certificates":[STANDARD.encode(cert.der())],"signature":STANDARD.encode(authority.sign(qtls::SignatureScheme::ECDSA_NISTP256_SHA256,&input).unwrap())});
    Writer::default()
        .write_raw_manifest(
            unsigned[index.manifest.span.start + 41..index.manifest.span.end()].to_vec(),
        )
        .write_atoms(atoms())
        .unwrap()
        .with_volume("metadata", metadata())
        .unwrap()
        .with_volume(
            "pishoo-signature",
            Directory::new(
                BTreeMap::from([(
                    "signature.json".parse().unwrap(),
                    serde_json::to_vec(&envelope).unwrap().into(),
                )]),
                Timestamps::default(),
            ),
        )
        .unwrap()
        .finish(SignatureAlgorithm::None)
        .unwrap()
        .to_vec()
}

#[test]
fn local_commands_use_default_or_explicit_identity_without_starting_a_server() {
    let root = tempfile::tempdir().unwrap();
    let home = &root.path().join(".dhttp");
    std::fs::create_dir_all(home).unwrap();
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
    let input = home.join("note.webc");
    std::fs::write(&input, component()).unwrap();
    let output = success(home, &["lib", "check", input.to_str().unwrap()]);
    let checked: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(checked["id"], "alice.dhttp.net/note");
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
    assert_eq!(info["endpoints"][0]["path"], "/std/upload");
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
    let home = &root.path().join(".dhttp");
    std::fs::create_dir_all(home).unwrap();
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

#[test]
fn enable_disable_commands_preserve_scope_and_report_saved_configuration() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join(".dhttp");
    std::fs::create_dir_all(&home).unwrap();
    initialize(&home, "alice");
    success(&home, &["listen", "--id", "alice", "external"]);
    let db = rusqlite::Connection::open(home.join("alice/db/config.db")).unwrap();
    for command in ["disable", "disable", "enable", "enable"] {
        let output = success(&home, &[command, "--id", "alice"]);
        let (listen, enabled) = db
            .query_row("SELECT listen, enabled FROM settings", [], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, bool>(1)?))
            })
            .unwrap();
        assert_eq!(listen, 2);
        assert_eq!(enabled, command == "enable");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("applies configuration automatically")
        );
        assert!(!String::from_utf8_lossy(&output.stderr).contains("Restart Pishoo"));
        assert!(!home.join("alice/db/chat.db").exists());
    }
    assert_eq!(
        run(&home, &["enable", "extra", "--id", "alice"])
            .status
            .code(),
        Some(2)
    );
    assert_eq!(
        run(&home, &["disable", "extra", "--id", "alice"])
            .status
            .code(),
        Some(2)
    );
}
