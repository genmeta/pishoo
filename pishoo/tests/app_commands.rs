//! Bootstrap a frontend through the real CLI before any Workspace is installed.
use std::{collections::BTreeMap, process::Command, sync::Arc};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::json;
use sha2::{Digest, Sha256};
use webc::v3::{
    SignatureAlgorithm, Timestamps,
    write::{Directory, Writer},
};

fn frontend(version: &str, text: &str) -> Vec<u8> {
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
    let manifest: webc::metadata::Manifest = serde_json::from_value(json!({"package":{
        "wapm":{"name":"alice.dhttp.net/hello","version":version,"description":"CLI bootstrap","license":"MIT"},
        "pishoo":{"kind":"app","title":"Frontend","dependencies":[],"entry":"/","changelog":"Test frontend; data unchanged"},
        "fs":[{"volume_name":"public","mount_path":"/public"}]}})).unwrap();
    let public = || {
        Directory::new(
            BTreeMap::from([(
                "index.html".parse().unwrap(),
                text.as_bytes().to_vec().into(),
            )]),
            Timestamps::default(),
        )
    };
    let unsigned = Writer::default()
        .write_manifest(&manifest)
        .unwrap()
        .write_atoms(BTreeMap::new())
        .unwrap()
        .with_volume("public", public())
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
    let envelope = json!({"version":1,"certificates":[STANDARD.encode(cert.der())],"signature":STANDARD.encode(authority.sign(qtls::SignatureScheme::ECDSA_NISTP256_SHA256,&input).unwrap())});
    Writer::default()
        .write_raw_manifest(
            unsigned[index.manifest.span.start + 41..index.manifest.span.end()].to_vec(),
        )
        .write_atoms(BTreeMap::new())
        .unwrap()
        .with_volume("public", public())
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
fn offline_app_check_install_query_update_and_remove_need_no_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let home = &dir.path().join(".dhttp");
    let profile = home.join("alice");
    std::fs::create_dir_all(profile.join("db")).unwrap();
    std::fs::create_dir(profile.join("libs")).unwrap();
    let db = rusqlite::Connection::open(profile.join("db/config.db")).unwrap();
    db.execute_batch("CREATE TABLE settings(listen INTEGER NOT NULL, enabled INTEGER NOT NULL); INSERT INTO settings VALUES(0,1); CREATE TABLE proxy_locations(location TEXT PRIMARY KEY, proxy_pass TEXT NOT NULL); PRAGMA user_version=1;").unwrap();
    std::fs::write(
        home.join("settings.toml"),
        "[default]\nname='alice.dhttp.net'\n",
    )
    .unwrap();
    let package = home.join("app.webc");
    std::fs::write(&package, frontend("1.0.0", "old page")).unwrap();
    let run = |args: &[&str]| {
        let result = Command::new(env!("CARGO_BIN_EXE_pishoo"))
            .env("HOME", home.parent().unwrap())
            .env("DHTTP_HOME", "/nonexistent-ignored-pishoo-home")
            .args(args)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        result
    };
    let checked: serde_json::Value =
        serde_json::from_slice(&run(&["app", "check", package.to_str().unwrap()]).stdout).unwrap();
    assert_eq!(checked["kind"], "app");
    let id = "alice.dhttp.net/hello";
    run(&["app", "install", id, package.to_str().unwrap()]);
    let info: serde_json::Value =
        serde_json::from_slice(&run(&["app", "info", id]).stdout).unwrap();
    assert_eq!(info["entry"], "/");
    assert_eq!(info["endpoints"], json!([]));
    assert!(info.get("openapi").is_none());
    assert!(profile.join("apps/hello/.webc-proof").is_file());
    assert!(!profile.join("libs/hello").exists());
    assert!(!profile.join("db/alice.dhttp.net/hello").exists());
    let items: serde_json::Value = serde_json::from_slice(&run(&["app", "list"]).stdout).unwrap();
    assert_eq!(items[0]["id"], id);
    std::fs::write(&package, frontend("2.0.0", "new page")).unwrap();
    run(&["app", "install", id, package.to_str().unwrap()]);
    let info: serde_json::Value =
        serde_json::from_slice(&run(&["app", "info", id]).stdout).unwrap();
    assert_eq!(info["version"], "2.0.0");
    run(&["app", "remove", id]);
    run(&["app", "remove", id]);
    let items: serde_json::Value = serde_json::from_slice(&run(&["app"]).stdout).unwrap();
    assert_eq!(items, json!([]));
}
