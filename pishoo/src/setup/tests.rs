use std::path::Path;

use super::*;

fn component(document: &str) -> Vec<u8> {
    fn leb(mut n: usize, out: &mut Vec<u8>) {
        loop {
            let b = (n & 127) as u8;
            n >>= 7;
            out.push(b | if n > 0 { 128 } else { 0 });
            if n == 0 {
                break;
            }
        }
    }
    let mut section = Vec::new();
    leb(14, &mut section);
    section.extend_from_slice(b"pishoo:openapi");
    section.extend_from_slice(document.as_bytes());
    let mut wasm = b"\0asm\x0d\0\x01\0".to_vec();
    wasm.push(0);
    leb(section.len(), &mut wasm);
    wasm.extend(section);
    wasm
}
fn profile(root: &Path) -> IdentityProfile {
    let profile = IdentityProfile::try_from(root.join("alice")).unwrap();
    std::fs::create_dir_all(profile.db_dir()).unwrap();
    let db = Connection::open(profile.db_dir().join("config.db")).unwrap();
    db.execute_batch("PRAGMA user_version=1; CREATE TABLE settings(listen INTEGER); INSERT INTO settings VALUES(1); CREATE TABLE proxy_locations(location TEXT,proxy_pass TEXT);").unwrap();
    profile
}
#[test]
fn config_requires_one_row_and_dhttp_upstreams() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    assert_eq!(load_server_config(&profile).unwrap().listen, 1);
    let db = Connection::open(profile.db_dir().join("config.db")).unwrap();
    db.execute(
        "INSERT INTO proxy_locations VALUES('/plain/','https://bob.dhttp.net')",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO proxy_locations VALUES('/replace/','https://bob.dhttp.net/')",
        [],
    )
    .unwrap();
    let config = load_server_config(&profile).unwrap();
    assert!(
        config.proxy_locations[0]
            .proxy_pass
            .path_and_query
            .is_none()
    );
    assert!(
        config.proxy_locations[1]
            .proxy_pass
            .path_and_query
            .is_some()
    );
    db.execute(
        "INSERT INTO proxy_locations VALUES('/shorthand','https://bob~/base')",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO proxy_locations VALUES('/self','https://~/')",
        [],
    )
    .unwrap();
    let expanded = load_server_config(&profile).unwrap();
    assert_eq!(
        expanded
            .proxy_locations
            .iter()
            .find(|p| p.location == "/shorthand")
            .unwrap()
            .proxy_pass
            .authority
            .as_ref()
            .unwrap(),
        "bob.dhttp.net"
    );
    assert_eq!(
        expanded
            .proxy_locations
            .iter()
            .find(|p| p.location == "/self")
            .unwrap()
            .proxy_pass
            .authority
            .as_ref()
            .unwrap(),
        "alice.dhttp.net"
    );
    db.execute(
        "INSERT INTO proxy_locations VALUES('/bad','https://example.com')",
        [],
    )
    .unwrap();
    assert!(load_server_config(&profile).is_err());
    db.execute("DELETE FROM proxy_locations", []).unwrap();
    db.execute("INSERT INTO settings VALUES(1)", []).unwrap();
    assert!(load_server_config(&profile).is_err());
}
#[test]
fn network_uses_union_and_off_is_no_network() {
    let off = ServerConfig {
        listen: 0,
        proxy_locations: vec![],
    };
    assert!(network_config(&[off]).unwrap().is_none());
    let configs = [
        ServerConfig {
            listen: 1,
            proxy_locations: vec![],
        },
        ServerConfig {
            listen: 2,
            proxy_locations: vec![],
        },
    ];
    let network = network_config(&configs).unwrap().unwrap();
    let dhttp::ListenConfig::Scope(scopes) = &network.listen[0] else {
        panic!()
    };
    assert!(scopes.contains(dhttp::Scope::Internal));
    assert!(scopes.contains(dhttp::Scope::External));
}
#[test]
fn manifest_rejects_duplicates_templates_references_and_reserved_routes() {
    let document = r#"{"openapi":"3.1.0","info":{"title":"test","version":"1"},"paths":{"/echo":{"get":{"responses":{"200":{"description":"ok"}}}}}}"#;
    let valid = component(document);
    assert!(validate_lib(&valid).is_ok());
    for broken in [
        document.replace("\"title\":\"test\"", "\"title\":\"a\",\"title\":\"b\""),
        document.replace("/echo", "/workspace"),
        document.replace("/echo", "/{id}"),
        document.replace(
            "\"get\":",
            "\"$ref\":\"https://example.com/schema\",\"get\":",
        ),
    ] {
        assert!(validate_lib(&component(&broken)).is_err());
    }
    let mut duplicate = valid.clone();
    duplicate.extend_from_slice(&valid[8..]);
    assert!(validate_lib(&duplicate).is_err());
}
