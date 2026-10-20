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
        authorization(
            &access,
            Method::POST,
            "/std/contact/application",
            Some("bob.dhttp.net")
        )
        .await,
        AuthResult::Allowed
    ));
    assert!(matches!(
        authorization(&access, Method::POST, "/std/contact/application", None).await,
        AuthResult::Denied
    ));
    for path in ["/std/contact/status", "/std/acl/status/12"] {
        assert!(matches!(
            authorization(&access, Method::GET, path, Some("bob.dhttp.net")).await,
            AuthResult::Allowed
        ));
        assert!(matches!(
            authorization(&access, Method::GET, path, None).await,
            AuthResult::Denied
        ));
    }
    for visitor in [None, Some("bob.dhttp.net")] {
        assert!(matches!(
            authorization(&access, Method::GET, "/std/acl/capabilities", visitor).await,
            AuthResult::Allowed
        ));
        for (method, path) in [
            (Method::GET, "/std/profile"),
            (Method::GET, "/std/profile/avatar"),
            (Method::GET, "/std/profile/context"),
            (Method::HEAD, "/std/profile/context"),
            (Method::GET, "/std/workspace"),
            (Method::GET, "/std/workspace/"),
            (Method::GET, "/std/workspace/assets/app.js"),
            (Method::HEAD, "/std/workspace/assets/app.css"),
        ] {
            assert!(
                matches!(
                    authorization(&access, method, path, visitor).await,
                    AuthResult::Allowed
                ),
                "public discovery must not require a contact or login: {path}"
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
            (Method::GET, "/std/contact/list"),
            (Method::PATCH, "/std/contact?name=bob.dhttp.net"),
            (Method::GET, "/std/contact/requests"),
            (Method::POST, "/std/message"),
            (Method::POST, "/std/workspace/"),
            (Method::GET, "/std/workspace-other/"),
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
            "/std/contact/application",
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
    for (method, path) in [
        (Method::GET, "/std/contact/status"),
        (Method::GET, "/std/acl/status"),
        (Method::GET, "/std/profile"),
        (Method::HEAD, "/std/profile/context"),
        (Method::GET, "/std/workspace"),
        (Method::HEAD, "/std/workspace"),
    ] {
        access
            .remove_policy(
                AccessMethod::Specified(method),
                path,
                if matches!(path, "/std/contact/status" | "/std/acl/status") {
                    Grantee::Named
                } else {
                    Grantee::All
                },
            )
            .await
            .unwrap();
    }
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
            "/std/contact/application",
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
        authorization(
            &access,
            Method::POST,
            "/std/contact/application",
            Some("bob.dhttp.net")
        )
        .await,
        AuthResult::Allowed
    ));
    assert_eq!(access_version(&profile.access_db_path()).unwrap(), Some(2));
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
            "published-legacy",
            "CREATE TABLE migration(version TEXT PRIMARY KEY,applied_at INTEGER); CREATE TABLE location_rule_sets(id INTEGER PRIMARY KEY,pattern TEXT,created_at TEXT,updated_at TEXT); CREATE TABLE location_rules(id INTEGER PRIMARY KEY,location_id INTEGER,action INTEGER,exprs TEXT,created_at TEXT,updated_at TEXT); INSERT INTO location_rule_sets VALUES(1,'/private','old','old'); INSERT INTO location_rules VALUES(1,1,1,'old-rule','old','old');",
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
async fn old_access_versions_upgrade_structure_without_replacing_data_or_seeding_rules() {
    let root = tempfile::tempdir().unwrap();
    for version in [0, 1] {
        let profile = profile(root.path(), &format!("v{version}"));
        let subject = SubjectId::new(b"owner").unwrap();
        let access = load_access(&profile, &subject).await.unwrap();
        drop(access);
        let db = rusqlite::Connection::open(profile.access_db_path()).unwrap();
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
            DROP TRIGGER contacts_updated;
            DROP TRIGGER contacts_inserted;
            DROP TRIGGER contacts_delete_exact_rules;
            DROP TABLE contact_deletions;
            DROP INDEX contacts_updated_at;
            CREATE TRIGGER contacts_delete_exact_rules AFTER DELETE ON contacts BEGIN
                DELETE FROM access_rules WHERE grantee=OLD.name;
            END;
            DELETE FROM access_rules;
            INSERT INTO contacts(name,subject_id,alias,status,expired_after,updated_at,created_at)
                VALUES('bob.dhttp.net',X'01','Bob',2,2000000000,100,100);
            INSERT INTO access_rules(method,api,effect,grantee_type,grantee,updated_at,created_at)
                VALUES('POST','/std/contact','deny',2,'**',42,42);",
        )
        .unwrap();
        if version == 0 {
            db.execute_batch("DROP TABLE contact_applications").unwrap();
        }
        db.execute("UPDATE module SET version=?1", [version])
            .unwrap();
        assert!(profile.db_dir().join("access.db-wal").exists());

        let reopened = load_access(&profile, &subject).await.unwrap();
        assert_eq!(access_version(&profile.access_db_path()).unwrap(), Some(2));
        assert_eq!(
            db.query_row("SELECT name,alias,updated_at FROM contacts", [], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .unwrap(),
            ("bob.dhttp.net".into(), "Bob".into(), 100)
        );
        assert_eq!(
            db.query_row(
                "SELECT method,api,effect,updated_at FROM access_rules",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                }
            )
            .unwrap(),
            ("POST".into(), "/std/contact".into(), "deny".into(), 42)
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM access_rules", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert!(matches!(
            authorization(
                &reopened,
                Method::GET,
                "/std/contact/status",
                Some("bob.dhttp.net")
            )
            .await,
            AuthResult::Denied
        ));
        assert!(!std::fs::read_dir(profile.db_dir()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("access-v0-backup-")
        }));
    }
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
