use std::{
    fs,
    path::{Path, PathBuf},
};

use dhttp::{ListenConfig, Scope};
use rusqlite::Connection;

use super::*;

fn leb(mut value: usize, out: &mut Vec<u8>) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
}

fn component_with_openapi() -> Vec<u8> {
    component_with_document(br#"{"openapi":"3.1.0","info":{"title":"Test","version":"1"},"paths":{"/health":{"get":{"operationId":"health"}}}}"#)
}

fn component_with_document(doc: &[u8]) -> Vec<u8> {
    let mut bytes =
        include_bytes!("../../tests/fixtures/wasi-http-read-request-then-respond.wasm").to_vec();
    let name = b"pishoo:openapi";
    let mut section = Vec::new();
    leb(name.len(), &mut section);
    section.extend_from_slice(name);
    section.extend_from_slice(doc);
    bytes.push(0);
    leb(section.len(), &mut bytes);
    bytes.extend_from_slice(&section);
    bytes
}

#[test]
fn retains_openapi_schemas_responses_and_extensions() {
    let document = serde_json::json!({
        "openapi":"3.1.0", "info":{"title":"Example","version":"1"},
        "components":{"schemas":{"User":{"type":"object"}}},
        "paths":{"/users":{"get":{
            "operationId":"users", "x-custom":true,
            "responses":{"200":{"description":"ok", "content":{"application/json":{
                "schema":{"$ref":"#/components/schemas/User"}
            }}}}
        }}}
    });
    let api = validate_lib(&component_with_document(
        &serde_json::to_vec(&document).unwrap(),
    ))
    .unwrap();
    assert_eq!(api.info.title, "Example");
    let saved = serde_json::to_value(api).unwrap();
    assert_eq!(saved["components"], document["components"]);
    assert_eq!(saved["paths"], document["paths"]);
}

#[test]
fn keeps_library_route_constraints_with_oas3() {
    for paths in [
        serde_json::json!({"/a":{"$ref":"#/components/pathItems/a"}}),
        serde_json::json!({"/../ssl":{"get":{}}}),
        serde_json::json!({"/a":{"get":{"operationId":"duplicate"}},"/b":{"post":{"operationId":"duplicate"}}}),
        serde_json::json!({"/a":{"get":false}}),
    ] {
        let document = serde_json::json!({
            "openapi":"3.1.0", "info":{"title":"Example","version":"1"}, "paths":paths
        });
        assert!(
            validate_lib(&component_with_document(
                &serde_json::to_vec(&document).unwrap()
            ))
            .is_err()
        );
    }
}

fn temp_root() -> PathBuf {
    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "pishoo-setup-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

fn database(server: &Path, listen: i64) {
    fs::create_dir_all(server.join("db")).unwrap();
    let db = Connection::open(server.join("db/config.db")).unwrap();
    db.execute_batch("PRAGMA user_version=1; CREATE TABLE settings (listen INTEGER); CREATE TABLE proxy_locations (location TEXT PRIMARY KEY, proxy_pass TEXT);").unwrap();
    db.execute("INSERT INTO settings VALUES (?1)", [listen])
        .unwrap();
}

#[test]
fn scans_only_valid_direct_server_directories_and_reads_config() {
    let root = temp_root();
    let alice = root.join("alice");
    let bad = root.join("Bad");
    let bob = root.join("bob");
    fs::create_dir_all(alice.join("ssl")).unwrap();
    fs::create_dir_all(bad.join("ssl")).unwrap();
    fs::create_dir_all(bob.join("ssl")).unwrap();
    database(&alice, 1);
    database(&bad, 2);
    database(&bob, 9);
    let found = scan(&root).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].name, "alice.dhttp.net");
    assert_eq!(found[0].config.listen, 1);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn rejects_multiple_or_non_integer_settings() {
    let root = temp_root();
    let profile = dhttp_home::identity::IdentityProfile::try_from(root.join("alice")).unwrap();
    database(profile.path(), 1);
    let db = Connection::open(profile.db_dir().join("config.db")).unwrap();
    db.execute("INSERT INTO settings VALUES (2)", []).unwrap();
    assert!(load_server_config(&profile).is_err());
    db.execute("DELETE FROM settings", []).unwrap();
    db.execute("INSERT INTO settings VALUES ('bad')", [])
        .unwrap();
    assert!(load_server_config(&profile).is_err());
    drop(db);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn maps_listen_bits_to_dhttp_scopes() {
    let config = |listen| ServerConfig {
        listen,
        proxy_locations: Vec::new(),
    };
    assert!(network_config(&[config(0)]).unwrap().is_none());
    assert!(network_config(&[config(4)]).is_err());
    let network = network_config(&[config(1), config(2)]).unwrap().unwrap();
    let ListenConfig::Scope(scopes) = &network.listen[0] else {
        panic!("expected scope rule")
    };
    assert!(scopes.contains(Scope::Internal));
    assert!(scopes.contains(Scope::External));
}

#[test]
fn scans_and_compiles_a_deployed_lib() {
    let root = temp_root();
    let server = root.join("alice");
    fs::create_dir_all(server.join("ssl")).unwrap();
    database(&server, 0);
    let lib = server.join("lib/profile");
    fs::create_dir_all(lib.join("data")).unwrap();
    fs::write(lib.join("lib.wasm"), component_with_openapi()).unwrap();
    let found = scan(&root).unwrap();
    assert_eq!(found[0].sandbox.libs().len(), 1);
    assert_eq!(
        found[0].sandbox.libs()[0].openapi.paths.as_ref().unwrap()["/health"]
            .get
            .as_ref()
            .unwrap()
            .operation_id
            .as_deref(),
        Some("health")
    );
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn ready_scan_skips_profile_without_credential_files() {
    let root = temp_root();
    let server = root.join("alice");
    fs::create_dir_all(server.join("ssl")).unwrap();
    database(&server, 1);
    let home = dhttp_home::DhttpHome::new(root.clone());

    assert_eq!(scan(&root).unwrap().len(), 1);
    assert!(scan_ready(&home).await.unwrap().is_empty());

    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn ready_scan_keeps_profile_with_complete_credentials() {
    let root = temp_root();
    let server = root.join("alice");
    let ssl = server.join("ssl");
    fs::create_dir_all(&ssl).unwrap();
    database(&server, 1);
    fs::write(
        ssl.join("fullchain.crt"),
        include_bytes!("../../../../dquic/tests/keychain/localhost/server.cert"),
    )
    .unwrap();
    let key = ssl.join("privkey.pem");
    fs::write(
        &key,
        include_bytes!("../../../../dquic/tests/keychain/localhost/server.key"),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&key, fs::Permissions::from_mode(0o400)).unwrap();
    }
    fs::write(ssl.join("ocsp.der"), b"staple").unwrap();
    let home = dhttp_home::DhttpHome::new(root.clone());

    let ready = scan_ready(&home).await.unwrap();
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].name, "alice.dhttp.net");

    fs::remove_dir_all(root).unwrap();
}
