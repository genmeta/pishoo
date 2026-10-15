use access_control::{AuthResult, Effect, Grantee, Headers, Method as AccessMethod, SubjectId};
use http::Method;

use super::*;

fn profile(root: &std::path::Path, name: &str) -> IdentityProfile {
    let profile = IdentityProfile::try_from(root.join(name)).unwrap();
    std::fs::create_dir_all(profile.path()).unwrap();
    initialize_directories(&profile).unwrap();
    profile
}

async fn authorization(
    access: &access_control::AccessService,
    method: Method,
    path: &str,
    name: Option<&str>,
) -> AuthResult {
    let subject = SubjectId::new(b"owner").unwrap();
    access
        .auth(
            Headers {
                method,
                path: path.to_owned(),
                fields: http::HeaderMap::new(),
            },
            name,
            name.map(|_| &subject),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn fresh_identity_initializes_native_databases_without_user_app_data() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path(), "alice");
    let config = load_server_config(&profile).unwrap();
    assert_eq!(config.listen, 3);
    let db = rusqlite::Connection::open(profile.config_db_path()).unwrap();
    let columns: Vec<String> = db
        .prepare("PRAGMA table_info(settings)")
        .unwrap()
        .query_map([], |row| row.get(1))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert_eq!(columns, ["listen", "enabled"]);
    assert!(config.enabled);
    assert!(config.proxy_locations.is_empty());
    for table in [
        "libs",
        "apps",
        "app_dependencies",
        "lib_dependencies",
        "lib_capabilities",
    ] {
        assert_eq!(
            db.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0,
            "new identities install business packages only on explicit request"
        );
    }
    let access = load_access(&profile, &SubjectId::new(b"owner").unwrap())
        .await
        .unwrap();
    for file in ["config.db", "access.db"] {
        assert!(profile.db_dir().join(file).is_file());
    }
    for directory in ["db", "file", "libs", "apps", "logs", "repo", "templates"] {
        assert!(profile.join(directory).is_dir());
    }
    assert!(!profile.join("server.conf").exists());
    assert!(
        !profile.join("ssl").exists(),
        "initialization does not manufacture identity credentials"
    );
    assert!(matches!(
        authorization(&access, Method::POST, "/std/contact", Some("bob.dhttp.net")).await,
        AuthResult::Allowed
    ));
    assert!(matches!(
        authorization(&access, Method::POST, "/std/contact", None).await,
        AuthResult::Denied
    ));
    for visitor in [None, Some("bob.dhttp.net")] {
        assert!(matches!(
            authorization(&access, Method::GET, "/std/acl/capabilities", visitor).await,
            AuthResult::Allowed
        ));
        for path in [
            "/std/profile",
            "/std/profile/avatar",
            "/std/profile/context",
        ] {
            let decision = authorization(&access, Method::GET, path, visitor).await;
            assert_eq!(
                matches!(decision, AuthResult::Allowed),
                visitor.is_some(),
                "profile discovery requires an authenticated identity: {path}"
            );
        }
        for (method, path) in [
            (Method::POST, "/std/acl/capabilities"),
            (Method::GET, "/std/acl/access"),
            (Method::GET, "/std/acl/allow"),
            (Method::GET, "/std/sys/settings"),
            (Method::PATCH, "/std/profile"),
            (Method::PUT, "/std/profile/avatar"),
            (Method::DELETE, "/std/profile/avatar"),
            (Method::GET, "/std/contacts"),
            (Method::PATCH, "/std/contact/bob.dhttp.net"),
            (Method::GET, "/std/contact/local/requests"),
            (Method::POST, "/std/message"),
        ] {
            assert!(
                matches!(
                    authorization(&access, method, path, visitor).await,
                    AuthResult::Denied
                ),
                "{path} must retain its existing authorization"
            );
        }
    }
    assert!(matches!(
        authorization(
            &access,
            Method::GET,
            "/std/workspace-api/context",
            Some(profile.name())
        )
        .await,
        AuthResult::Allowed
    ));
    let db = rusqlite::Connection::open(profile.access_db_path()).unwrap();
    for table in ["contacts", "contact_applications", "access_reviews"] {
        assert_eq!(
            db.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    assert!(!profile.db_dir().join("workspace.db").exists());
    assert!(!profile.db_dir().join("chat.db").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for file in ["config.db", "access.db"] {
            assert_eq!(
                profile
                    .db_dir()
                    .join(file)
                    .metadata()
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert_eq!(
            profile.db_dir().metadata().unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
}

#[tokio::test]
async fn reopening_preserves_removed_default_rule_and_existing_config() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path(), "alice");
    load_server_config(&profile).unwrap();
    let subject = SubjectId::new(b"owner").unwrap();
    let access = load_access(&profile, &subject).await.unwrap();
    access
        .remove_policy(
            AccessMethod::Specified(Method::POST),
            "/std/contact",
            Grantee::Named,
        )
        .await
        .unwrap();
    access
        .remove_policy(
            AccessMethod::Specified(Method::GET),
            "/std/acl/capabilities",
            Grantee::All,
        )
        .await
        .unwrap();
    access
        .remove_policy(
            AccessMethod::Specified(Method::GET),
            "/std/profile",
            Grantee::Named,
        )
        .await
        .unwrap();
    let db = rusqlite::Connection::open(profile.config_db_path()).unwrap();
    db.execute_batch("UPDATE settings SET listen=1; INSERT INTO proxy_locations VALUES('/service','127.0.0.1:8080');").unwrap();
    let config = load_server_config(&profile).unwrap();
    assert_eq!(config.listen, 1);
    assert_eq!(config.proxy_locations.len(), 1);
    let reopened = load_access(&profile, &subject).await.unwrap();
    assert!(matches!(
        authorization(
            &reopened,
            Method::POST,
            "/std/contact",
            Some("bob.dhttp.net")
        )
        .await,
        AuthResult::Denied
    ));
    assert!(matches!(
        authorization(
            &reopened,
            Method::GET,
            "/std/acl/capabilities",
            Some("bob.dhttp.net")
        )
        .await,
        AuthResult::Denied
    ));
    assert_eq!(
        rusqlite::Connection::open(profile.access_db_path())
            .unwrap()
            .query_row("SELECT count(*) FROM access_rules", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn empty_access_file_and_abandoned_staging_do_not_lose_initial_rules() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path(), "alice");
    std::fs::write(profile.access_db_path(), []).unwrap();
    let abandoned = profile.db_dir().join(".access-init-abandoned");
    std::fs::create_dir(&abandoned).unwrap();
    std::fs::write(abandoned.join("partial.db"), b"partial").unwrap();
    let access = load_access(&profile, &SubjectId::new(b"owner").unwrap())
        .await
        .unwrap();
    assert!(matches!(
        authorization(&access, Method::POST, "/std/contact", Some("bob.dhttp.net")).await,
        AuthResult::Allowed
    ));
    assert_eq!(access_version(&profile.access_db_path()).unwrap(), Some(1));
}

#[tokio::test]
async fn unsupported_access_formats_are_preserved_without_adding_tables() {
    let root = tempfile::tempdir().unwrap();
    for (name, contents) in [
        (
            "legacy",
            "CREATE TABLE location_rule_sets(id INTEGER PRIMARY KEY,pattern TEXT); CREATE TABLE location_rules(id INTEGER); INSERT INTO location_rule_sets VALUES(1,'/');",
        ),
        (
            "unknown",
            "CREATE TABLE important(value TEXT); INSERT INTO important VALUES('keep');",
        ),
        (
            "future",
            "CREATE TABLE module(module_name TEXT,version INTEGER); INSERT INTO module VALUES('access',99);",
        ),
    ] {
        let profile = profile(root.path(), name);
        let path = profile.access_db_path();
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch(contents)
            .unwrap();
        let before = std::fs::read(&path).unwrap();
        let error = load_access(&profile, &SubjectId::new(b"owner").unwrap())
            .await
            .err()
            .expect("unsupported format must fail");
        assert!(
            error.to_string().contains(path.to_str().unwrap()),
            "{error}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
    let profile = profile(root.path(), "corrupt");
    std::fs::write(profile.access_db_path(), b"not sqlite").unwrap();
    assert!(
        load_access(&profile, &SubjectId::new(b"owner").unwrap())
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read(profile.access_db_path()).unwrap(),
        b"not sqlite"
    );
}

#[tokio::test]
async fn published_legacy_configuration_is_backed_up_and_not_imported() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path(), "alice");
    let db = rusqlite::Connection::open(profile.access_db_path()).unwrap();
    db.execute_batch("CREATE TABLE migration(version TEXT PRIMARY KEY,applied_at INTEGER); INSERT INTO migration VALUES('m20250909_154000_create_table',1);
        CREATE TABLE location_rule_sets(id INTEGER PRIMARY KEY,pattern TEXT,created_at TEXT,updated_at TEXT);
        CREATE TABLE location_rules(id INTEGER PRIMARY KEY,location_id INTEGER,action INTEGER,exprs TEXT,created_at TEXT,updated_at TEXT);
        INSERT INTO location_rule_sets VALUES(1,'/private','old','old');
        INSERT INTO location_rules VALUES(1,1,1,'old-rule','old','old');").unwrap();
    drop(db);
    let access = load_access(&profile, &SubjectId::new(b"owner").unwrap())
        .await
        .unwrap();
    assert!(matches!(
        authorization(&access, Method::POST, "/std/contact", Some("bob.dhttp.net")).await,
        AuthResult::Allowed
    ));
    let current = rusqlite::Connection::open(profile.access_db_path()).unwrap();
    assert_eq!(
        current
            .query_row("SELECT count(*) FROM contacts", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        current
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='location_rules'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    let backups = std::fs::read_dir(profile.db_dir())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("access-legacy-backup-")
        })
        .collect::<Vec<_>>();
    assert_eq!(backups.len(), 1);
    let backup = rusqlite::Connection::open(&backups[0]).unwrap();
    assert_eq!(
        backup
            .query_row("SELECT exprs FROM location_rules WHERE id=1", [], |row| row
                .get::<_, String>(0))
            .unwrap(),
        "old-rule"
    );
    load_access(&profile, &SubjectId::new(b"owner").unwrap())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_dir(profile.db_dir())
            .unwrap()
            .filter(|entry| entry
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("access-legacy-backup-"))
            .count(),
        1
    );
}

#[tokio::test]
async fn old_access_v0_is_archived_without_importing_its_policies() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path(), "alice");
    let subject = SubjectId::new(b"owner").unwrap();
    let access = load_access(&profile, &subject).await.unwrap();
    access
        .set_policy(
            AccessMethod::Specified(Method::POST),
            "/std/contact",
            Effect::Deny,
            Grantee::Named,
        )
        .await
        .unwrap();
    drop(access);
    let db = rusqlite::Connection::open(profile.access_db_path()).unwrap();
    db.execute_batch("PRAGMA journal_mode=WAL; DROP TABLE contact_applications; UPDATE module SET version=0; UPDATE access_rules SET updated_at=42;")
        .unwrap();
    assert!(profile.db_dir().join("access.db-wal").exists());
    let reopened = load_access(&profile, &subject).await.unwrap();
    assert!(matches!(
        authorization(
            &reopened,
            Method::POST,
            "/std/contact",
            Some("bob.dhttp.net")
        )
        .await,
        AuthResult::Allowed
    ));
    assert_eq!(access_version(&profile.access_db_path()).unwrap(), Some(1));
    let backups = std::fs::read_dir(profile.db_dir())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("access-v0-backup-")
        })
        .collect::<Vec<_>>();
    assert_eq!(backups.len(), 1);
    assert_eq!(access_version(&backups[0]).unwrap(), Some(0));
    assert_eq!(
        rusqlite::Connection::open(&backups[0])
            .unwrap()
            .query_row(
                "SELECT effect FROM access_rules WHERE api='/std/contact'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        "deny"
    );
    assert_eq!(
        rusqlite::Connection::open(&backups[0])
            .unwrap()
            .query_row(
                "SELECT updated_at FROM access_rules WHERE api='/std/contact'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        42
    );
    load_access(&profile, &subject).await.unwrap();
    assert_eq!(
        std::fs::read_dir(profile.db_dir())
            .unwrap()
            .filter(|entry| entry
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("access-v0-backup-"))
            .count(),
        1
    );
}

#[test]
fn unknown_config_is_preserved_and_empty_config_recovers() {
    let root = tempfile::tempdir().unwrap();
    let empty = profile(root.path(), "empty");
    std::fs::write(empty.config_db_path(), []).unwrap();
    assert_eq!(load_server_config(&empty).unwrap().listen, 3);
    for (name, sql) in [
        (
            "unknown",
            "CREATE TABLE important(value); INSERT INTO important VALUES('keep');",
        ),
        ("future", "PRAGMA user_version=99;"),
        (
            "partial",
            "CREATE TABLE settings(listen); INSERT INTO settings VALUES(1);",
        ),
    ] {
        let profile = profile(root.path(), name);
        rusqlite::Connection::open(profile.config_db_path())
            .unwrap()
            .execute_batch(sql)
            .unwrap();
        let before = std::fs::read(profile.config_db_path()).unwrap();
        assert!(load_server_config(&profile).is_err());
        assert_eq!(std::fs::read(profile.config_db_path()).unwrap(), before);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn initialization_rejects_symlinked_directories_and_databases() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let external = root.path().join("external");
    std::fs::create_dir(&external).unwrap();
    let identity = IdentityProfile::try_from(root.path().join("linked")).unwrap();
    std::fs::create_dir(identity.path()).unwrap();
    symlink(&external, identity.db_dir()).unwrap();
    assert!(initialize_directories(&identity).is_err());
    assert!(load_server_config(&identity).is_err());
    assert_eq!(std::fs::read_dir(&external).unwrap().count(), 0);
    let profile = profile(root.path(), "alice");
    let original = external.join("data");
    std::fs::write(&original, b"untouched").unwrap();
    for file in ["config.db", "access.db"] {
        symlink(&original, profile.db_dir().join(file)).unwrap();
    }
    assert!(load_server_config(&profile).is_err());
    assert!(
        load_access(&profile, &SubjectId::new(b"owner").unwrap())
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(&original).unwrap(), b"untouched");
}
