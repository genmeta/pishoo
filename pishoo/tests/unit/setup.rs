use std::path::Path;

use dhttp_home::identity::IdentityProfile;
use rusqlite::Connection;

use super::*;

fn profile(root: &Path) -> IdentityProfile {
    let profile = IdentityProfile::try_from(root.join("alice")).unwrap();
    std::fs::create_dir_all(profile.db_dir()).unwrap();
    let db = Connection::open(profile.db_dir().join("config.db")).unwrap();
    db.execute_batch("PRAGMA user_version=1; CREATE TABLE settings(listen INTEGER, ssh INTEGER); INSERT INTO settings VALUES(1,0); CREATE TABLE proxy_locations(location TEXT,proxy_pass TEXT);").unwrap();
    profile
}
#[test]
fn config_requires_one_row_and_dhttp_upstreams() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    assert_eq!(load_server_config(&profile).unwrap().listen, 1);
    assert!(!load_server_config(&profile).unwrap().ssh);
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
    db.execute("INSERT INTO settings VALUES(1,0)", []).unwrap();
    assert!(load_server_config(&profile).is_err());
}
#[test]
fn ssh_is_a_strict_server_setting() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path());
    let db = Connection::open(profile.db_dir().join("config.db")).unwrap();
    db.execute("UPDATE settings SET ssh=1", []).unwrap();
    assert!(load_server_config(&profile).unwrap().ssh);
    db.execute("UPDATE settings SET ssh=2", []).unwrap();
    assert!(matches!(
        load_server_config(&profile),
        Err(Error::InvalidConfig(_))
    ));
    db.execute("UPDATE settings SET ssh='on'", []).unwrap();
    assert!(matches!(
        load_server_config(&profile),
        Err(Error::InvalidConfig(_))
    ));
}
