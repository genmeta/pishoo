use std::path::Path;

use dhttp_home::identity::IdentityProfile;
use rusqlite::Connection;

use super::*;

fn profile(root: &Path) -> IdentityProfile {
    let profile = IdentityProfile::try_from(root.join("alice")).unwrap();
    std::fs::create_dir_all(profile.db_dir()).unwrap();
    let db = Connection::open(profile.db_dir().join("config.db")).unwrap();
    db.execute_batch("PRAGMA user_version=1; CREATE TABLE settings(listen INTEGER, exec INTEGER); INSERT INTO settings VALUES(1,0); CREATE TABLE proxy_locations(location TEXT,proxy_pass TEXT);").unwrap();
    profile
}
#[test]
fn config_requires_one_row_and_local_http_upstreams() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    assert_eq!(load_server_config(&profile).unwrap().listen, 1);
    assert!(!load_server_config(&profile).unwrap().exec);
    let db = Connection::open(profile.db_dir().join("config.db")).unwrap();
    db.execute(
        "INSERT INTO proxy_locations VALUES('/plain/','http://127.0.0.1:8080')",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO proxy_locations VALUES('/replace/','http://127.0.0.1:8080/')",
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
        "INSERT INTO proxy_locations VALUES('/shorthand','127.0.0.1:8080')",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO proxy_locations VALUES('/ipv6','http://[::1]:8081/')",
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
        "127.0.0.1:8080"
    );
    assert!(
        expanded
            .proxy_locations
            .iter()
            .find(|p| p.location == "/shorthand")
            .unwrap()
            .proxy_pass
            .path_and_query
            .is_none()
    );
    assert_eq!(
        expanded
            .proxy_locations
            .iter()
            .find(|p| p.location == "/ipv6")
            .unwrap()
            .proxy_pass
            .authority
            .as_ref()
            .unwrap(),
        "[::1]:8081"
    );
    db.execute(
        "INSERT INTO proxy_locations VALUES('/bad','https://bob.dhttp.net')",
        [],
    )
    .unwrap();
    assert!(load_server_config(&profile).is_err());
    db.execute("DELETE FROM proxy_locations WHERE location='/bad'", [])
        .unwrap();
    for target in [
        "http://example.com:8080",
        "http://192.168.1.2:8080",
        "https://127.0.0.1:8080",
    ] {
        db.execute("INSERT INTO proxy_locations VALUES('/bad',?1)", [target])
            .unwrap();
        assert!(load_server_config(&profile).is_err(), "{target}");
        db.execute("DELETE FROM proxy_locations WHERE location='/bad'", [])
            .unwrap();
    }
    db.execute("DELETE FROM proxy_locations", []).unwrap();
    db.execute("INSERT INTO settings VALUES(1,0)", []).unwrap();
    assert!(load_server_config(&profile).is_err());
}
#[test]
fn exec_is_a_strict_server_setting() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    let db = Connection::open(profile.db_dir().join("config.db")).unwrap();
    db.execute("UPDATE settings SET exec=1", []).unwrap();
    assert!(load_server_config(&profile).unwrap().exec);
    db.execute("UPDATE settings SET exec=2", []).unwrap();
    assert!(matches!(
        load_server_config(&profile),
        Err(Error::InvalidConfig(_))
    ));
    db.execute("UPDATE settings SET exec='on'", []).unwrap();
    assert!(matches!(
        load_server_config(&profile),
        Err(Error::InvalidConfig(_))
    ));
}

#[test]
fn file_namespace_cannot_be_proxied() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    let db = Connection::open(profile.db_dir().join("config.db")).unwrap();
    for location in ["/file", "/file/", "= /file/a"] {
        db.execute("DELETE FROM proxy_locations", []).unwrap();
        db.execute(
            "INSERT INTO proxy_locations VALUES(?1,'127.0.0.1:8080')",
            [location],
        )
        .unwrap();
        assert!(matches!(
            load_server_config(&profile),
            Err(Error::InvalidConfig(_))
        ));
    }
}
