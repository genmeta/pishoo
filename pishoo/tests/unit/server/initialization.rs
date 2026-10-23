use access_control::{AuthResult, Grantee, Headers, Method as AccessMethod, SubjectId};
use http::Method;

use super::{access::access_version, *};

#[test]
#[ignore = "requires root; run in an isolated filesystem"]
fn root_initialization_repairs_identity_directories_and_existing_packages() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    assert!(rustix::process::geteuid().is_root());
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("alice")).unwrap();
    std::fs::create_dir_all(profile.join("app/hello/public")).unwrap();
    std::fs::create_dir_all(profile.join("lib/std/hello")).unwrap();
    let index = profile.join("app/hello/public/index.html");
    std::fs::write(&index, b"page").unwrap();
    std::fs::set_permissions(&index, std::fs::Permissions::from_mode(0o600)).unwrap();
    let outside = root.path().join("outside");
    std::fs::write(&outside, b"untouched").unwrap();
    symlink(&outside, profile.join("app/symlink")).unwrap();
    std::fs::hard_link(&outside, profile.join("lib/hardlink")).unwrap();
    let identity = std::fs::File::open(profile.path()).unwrap();
    rustix::fs::fchown(
        &identity,
        Some(rustix::fs::Uid::from_raw(10001)),
        Some(rustix::fs::Gid::from_raw(10002)),
    )
    .unwrap();

    for _ in 0..2 {
        initialize_directories(&profile).unwrap();
        for name in [
            "",
            ".volume",
            ".volume/sys",
            ".volume/sys/setting",
            ".volume/sys/pkg",
            ".volume/sys/access",
            ".volume/sys/contact",
            ".volume/std",
            "lib",
            "lib/std",
            "lib/std/hello",
            "app",
            "app/hello",
            "app/hello/index.html",
            "log",
            "repo",
        ] {
            let metadata = profile.join(name).metadata().unwrap();
            assert_eq!((metadata.uid(), metadata.gid()), (10001, 10002), "{name}");
        }
        assert_eq!(
            profile
                .join("app/hello/index.html")
                .metadata()
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::read(profile.join("app/hello/index.html")).unwrap(),
            b"page"
        );
        assert_eq!(outside.metadata().unwrap().uid(), 0);
    }
}

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
    assert_eq!(config.listen, "off");
    let db =
        crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    let columns: Vec<String> = db
        .prepare("PRAGMA table_info(setting)")
        .unwrap()
        .query_map([], |row| row.get(1))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert_eq!(columns, ["key", "value"]);
    assert_eq!(config.listen, "off");
    assert!(config.proxy_locations.is_empty());
    for table in ["lib", "app", "package_dependency", "lib_capability"] {
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
    for name in ["setting", "access", "pkg"] {
        assert!(
            profile
                .join(format!(".volume/sys/{name}/database"))
                .is_file()
        );
    }
    for directory in [".volume", "lib", "app", "log", "repo"] {
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
    for path in ["/std/contact", "/std/acl/status?id=12"] {
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
            authorization(&access, Method::GET, "/std/acl/permissions", visitor).await,
            AuthResult::Allowed
        ));
        for (method, path) in [
            (Method::GET, "/std/profile/nickname"),
            (Method::GET, "/std/profile/avatar"),
            (Method::GET, "/app/introduction"),
            (Method::GET, "/app/introduction/"),
            (Method::GET, "/app/introduction/assets/app.js"),
            (Method::GET, "/app/introduction/assets/app.css"),
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
            (Method::POST, "/std/acl/permissions"),
            (Method::GET, "/std/profile"),
            (Method::GET, "/std/profile/introduction"),
            (Method::GET, "/std/profile/permissions"),
            (Method::GET, "/std/profile/permission-sets"),
            (Method::GET, "/app/workspace"),
            (Method::GET, "/app/workspace/assets/app.js"),
            (Method::GET, "/app/introduction-other/"),
            (Method::GET, "/std/acl/access"),
            (Method::GET, "/std/acl/allow"),
            (Method::GET, "/std/sys/listen"),
            (Method::PUT, "/std/profile/nickname"),
            (Method::PUT, "/std/profile/avatar"),
            (Method::DELETE, "/std/profile/avatar"),
            (Method::GET, "/std/contact/list"),
            (Method::PATCH, "/std/contact?name=bob.dhttp.net"),
            (Method::PUT, "/std/message"),
            (Method::POST, "/app/introduction/"),
            (Method::GET, "/app/workspace-other/"),
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
    let db = rusqlite::Connection::open(profile.join(".volume/sys/access/database")).unwrap();
    for table in ["contacts", "contact_applications", "access_reviews"] {
        assert_eq!(
            db.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    assert!(
        !profile
            .join(".volume/sys/setting")
            .join("workspace.db")
            .exists()
    );
    assert!(!profile.join(".volume/sys/setting").join("chat.db").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for file in [
            ".volume/sys/setting/database",
            ".volume/sys/access/database",
        ] {
            assert_eq!(
                profile.join(file).metadata().unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(
            profile
                .join(".volume/sys/setting")
                .metadata()
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}

#[tokio::test]
async fn configurable_defaults_seed_only_new_databases() {
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path(), "alice");
    let defaults = profile.join(".volume/sys/access/defaults.json");
    std::fs::write(
        &defaults,
        r#"{"custom.example/card":[["GET","/app/card","allow","*?"]]}"#,
    )
    .unwrap();
    let subject = SubjectId::new(b"owner").unwrap();
    let access = load_access(&profile, &subject).await.unwrap();
    assert!(matches!(
        authorization(&access, Method::GET, "/app/card", None).await,
        AuthResult::Allowed
    ));
    assert!(matches!(
        authorization(&access, Method::GET, "/app/introduction", None).await,
        AuthResult::Denied
    ));
    access
        .set_policy(
            AccessMethod::Specified(Method::GET),
            "/app/card",
            access_control::Effect::Deny,
            Grantee::All,
        )
        .await
        .unwrap();
    // Even a broken template must not override or prevent loading existing ACLs.
    std::fs::write(defaults, "invalid JSON").unwrap();
    let reopened = load_access(&profile, &subject).await.unwrap();
    assert!(matches!(
        authorization(&reopened, Method::GET, "/app/card", None).await,
        AuthResult::Denied
    ));
}

#[tokio::test]
async fn invalid_defaults_never_publish_partial_authorization() {
    let root = tempfile::tempdir().unwrap();
    for value in [
        "not JSON",
        r#"{"native":[["GET","/public","allow","*?"],["get","/private","allow","*?"]]}"#,
        r#"{"native":[["GET","/public?field=secret","allow","*?"]]}"#,
        r#"{"native":[["GET","/public","invalid","*?"]]}"#,
    ] {
        let profile = profile(root.path(), "alice");
        std::fs::write(profile.join(".volume/sys/access/defaults.json"), value).unwrap();
        assert!(
            load_access(&profile, &SubjectId::new(b"owner").unwrap())
                .await
                .is_err()
        );
        assert!(!profile.join(".volume/sys/access/database").exists());
    }
    let profile = profile(root.path(), "alice");
    std::fs::write(profile.join(".volume/sys/access/defaults.json"), "{}").unwrap();
    let access = load_access(&profile, &SubjectId::new(b"owner").unwrap())
        .await
        .unwrap();
    assert!(matches!(
        authorization(&access, Method::GET, "/std/profile/nickname", None).await,
        AuthResult::Denied
    ));
    assert!(matches!(
        authorization(&access, Method::GET, "/app/workspace", Some(profile.name())).await,
        AuthResult::Allowed
    ));
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
            "/std/acl/permissions",
            Grantee::All,
        )
        .await
        .unwrap();
    for (method, path) in [
        (Method::GET, "/std/contact"),
        (Method::GET, "/std/contact/list"),
        (Method::GET, "/std/acl/status"),
        (Method::GET, "/std/profile/nickname"),
        (Method::GET, "/std/profile/avatar"),
        (Method::GET, "/app/workspace"),
        (Method::GET, "/app/introduction"),
        (Method::HEAD, "/app/introduction"),
    ] {
        access
            .remove_policy(
                AccessMethod::Specified(method),
                path,
                if matches!(path, "/std/contact" | "/std/acl/status") {
                    Grantee::Named
                } else {
                    Grantee::All
                },
            )
            .await
            .unwrap();
    }
    let db =
        crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    db.execute_batch("INSERT INTO setting VALUES('listen','internal'); INSERT INTO proxy_location VALUES('/service','127.0.0.1:8080');").unwrap();
    let config = load_server_config(&profile).unwrap();
    assert_eq!(config.listen, "internal");
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
            "/std/acl/permissions",
            Some("bob.dhttp.net")
        )
        .await,
        AuthResult::Denied
    ));
    assert_eq!(
        rusqlite::Connection::open(profile.join(".volume/sys/access/database"))
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
    std::fs::write(profile.join(".volume/sys/access/database"), []).unwrap();
    let abandoned = profile
        .join(".volume/sys/setting")
        .join(".access-init-abandoned");
    std::fs::create_dir(&abandoned).unwrap();
    std::fs::write(abandoned.join("partial.db"), b"partial").unwrap();
    let access = load_access(&profile, &SubjectId::new(b"owner").unwrap())
        .await
        .unwrap();
    assert!(matches!(
        authorization(&access, Method::POST, "/std/contact", Some("bob.dhttp.net")).await,
        AuthResult::Allowed
    ));
    assert_eq!(
        access_version(&profile.join(".volume/sys/access/database")).unwrap(),
        Some(3)
    );
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
        let path = profile.join(".volume/sys/access/database");
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
    std::fs::write(profile.join(".volume/sys/access/database"), b"not sqlite").unwrap();
    assert!(
        load_access(&profile, &SubjectId::new(b"owner").unwrap())
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read(profile.join(".volume/sys/access/database")).unwrap(),
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
        let db = rusqlite::Connection::open(profile.join(".volume/sys/access/database")).unwrap();
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
            DROP TRIGGER access_reviews_policy_insert;
            DROP TRIGGER access_reviews_policy_update;
            DROP TRIGGER access_reviews_policy_delete;
            DROP TRIGGER access_reviews_contact_update;
            DROP TRIGGER access_reviews_contact_delete;
            DROP TRIGGER access_reviews_contact_insert;
            DROP INDEX access_reviews_once;
            DROP INDEX access_reviews_pending_scope;
            ALTER TABLE access_reviews DROP COLUMN remember;
            ALTER TABLE access_reviews DROP COLUMN decided_at;
            ALTER TABLE access_reviews DROP COLUMN grant_expires_at;
            ALTER TABLE access_reviews DROP COLUMN consumed_at;
            ALTER TABLE access_reviews DROP COLUMN invalidated_at;
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
        assert!(profile.join(".volume/sys/access/database-wal").exists());

        let reopened = load_access(&profile, &subject).await.unwrap();
        assert_eq!(
            access_version(&profile.join(".volume/sys/access/database")).unwrap(),
            Some(3)
        );
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
                "/std/contact",
                Some("bob.dhttp.net")
            )
            .await,
            AuthResult::Denied
        ));
        assert!(
            !std::fs::read_dir(profile.join(".volume/sys/setting"))
                .unwrap()
                .any(|entry| {
                    entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with("access-v0-backup-")
                })
        );
    }
}

#[test]
fn unknown_config_is_preserved_and_empty_config_recovers() {
    let root = tempfile::tempdir().unwrap();
    let empty = profile(root.path(), "empty");
    std::fs::write(empty.join(".volume/sys/setting/database"), []).unwrap();
    assert_eq!(load_server_config(&empty).unwrap().listen, "off");
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
        rusqlite::Connection::open(profile.join(".volume/sys/setting/database"))
            .unwrap()
            .execute_batch(sql)
            .unwrap();
        let before = std::fs::read(profile.join(".volume/sys/setting/database")).unwrap();
        assert!(load_server_config(&profile).is_err());
        assert_eq!(
            std::fs::read(profile.join(".volume/sys/setting/database")).unwrap(),
            before
        );
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
    symlink(&external, identity.join(".volume")).unwrap();
    assert!(initialize_directories(&identity).is_err());
    assert!(load_server_config(&identity).is_err());
    assert_eq!(std::fs::read_dir(&external).unwrap().count(), 0);
    let profile = profile(root.path(), "alice");
    let original = external.join("data");
    std::fs::write(&original, b"untouched").unwrap();
    for file in [
        ".volume/sys/setting/database",
        ".volume/sys/access/database",
    ] {
        symlink(&original, profile.join(file)).unwrap();
    }
    assert!(load_server_config(&profile).is_err());
    assert!(
        load_access(&profile, &SubjectId::new(b"owner").unwrap())
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(&original).unwrap(), b"untouched");
}

#[tokio::test]
async fn native_resource_handles_keep_the_personal_account_owner() {
    use std::{io::Write, os::unix::fs::MetadataExt};
    let root = tempfile::tempdir().unwrap();
    let account = root.path().join("account");
    let profile = IdentityProfile::try_from(account.join(".dhttp/alice")).unwrap();
    std::fs::create_dir_all(profile.ssl_dir()).unwrap();
    initialize_directories(&profile).unwrap();
    load_server_config(&profile).unwrap();
    let access = load_access(&profile, &SubjectId::new(b"owner").unwrap())
        .await
        .unwrap();
    let contact = crate::contact::database(&profile).unwrap();
    let mut log = open_access_log(&profile).unwrap();
    writeln!(log, "owned access log").unwrap();
    cache_ocsp(&profile, b"cached response").unwrap();
    let owner = account.metadata().unwrap();
    for path in [
        profile.path().to_path_buf(),
        profile.join(".volume/sys/setting/database"),
        profile.join(".volume/sys/access/database"),
        profile.join(".volume/sys/contact/database"),
        profile.join("log/access.log"),
        profile.ocsp_path(),
    ] {
        let metadata = path.metadata().unwrap();
        assert_eq!(
            (metadata.uid(), metadata.gid()),
            (owner.uid(), owner.gid()),
            "{}",
            path.display()
        );
    }
    drop((contact, access, log));
}

#[test]
fn contact_database_links_do_not_modify_external_files() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path(), "alice");
    let outside = root.path().join("outside.db");
    std::fs::write(&outside, b"external database bytes").unwrap();
    std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o640)).unwrap();
    let original = outside.metadata().unwrap();
    for hardlink in [false, true] {
        let path = profile.join(".volume/sys/contact/database");
        if hardlink {
            std::fs::hard_link(&outside, &path).unwrap();
        } else {
            symlink(&outside, &path).unwrap();
        }
        assert!(crate::contact::database(&profile).is_err());
        let metadata = outside.metadata().unwrap();
        assert_eq!(std::fs::read(&outside).unwrap(), b"external database bytes");
        assert_eq!(metadata.mode(), original.mode());
        assert_eq!(
            (metadata.uid(), metadata.gid()),
            (original.uid(), original.gid())
        );
        std::fs::remove_file(path).unwrap();
    }
}

#[tokio::test]
#[ignore = "requires root; run in an isolated filesystem"]
async fn root_personal_home_repairs_initially_root_owned_native_resources() {
    use std::{
        io::Write,
        os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    };
    assert!(rustix::process::geteuid().is_root());
    let root = tempfile::tempdir().unwrap();
    let account = root.path().join("account");
    let profile = IdentityProfile::try_from(account.join(".dhttp/alice")).unwrap();
    std::fs::create_dir_all(profile.ssl_dir()).unwrap();
    for directory in ["log", "cache", "browser-only/ssl"] {
        std::fs::create_dir_all(account.join(".dhttp").join(directory)).unwrap();
    }
    for relative in [
        "log/error.log",
        "cache/catalogue",
        "browser-only/ssl/fullchain.crt",
        "browser-only/ssl/privkey.pem",
    ] {
        std::fs::write(
            account.join(".dhttp").join(relative),
            b"retained account data",
        )
        .unwrap();
    }
    std::fs::set_permissions(
        account.join(".dhttp/browser-only/ssl/privkey.pem"),
        std::fs::Permissions::from_mode(0o400),
    )
    .unwrap();
    std::fs::write(
        profile.ssl_dir().join("fullchain.crt"),
        b"retained credentials",
    )
    .unwrap();
    let outside = root.path().join("outside");
    std::fs::write(&outside, b"untouched").unwrap();
    std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o640)).unwrap();
    std::fs::create_dir_all(profile.join("file")).unwrap();
    symlink(&outside, profile.join("file/symlink")).unwrap();
    std::fs::hard_link(&outside, profile.join("file/hardlink")).unwrap();
    let outside_owner = outside.metadata().unwrap();
    let account_file = std::fs::File::open(&account).unwrap();
    rustix::fs::fchown(
        &account_file,
        Some(rustix::fs::Uid::from_raw(10001)),
        Some(rustix::fs::Gid::from_raw(10002)),
    )
    .unwrap();
    assert_eq!(profile.path().metadata().unwrap().uid(), 0);
    initialize_directories(&profile).unwrap();
    load_server_config(&profile).unwrap();
    let config =
        crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    config.pragma_update(None, "journal_mode", "WAL").unwrap();
    crate::setup::config_database(
        &profile,
        &Method::PATCH,
        &http::Uri::from_static("/std/sys/listen"),
        Some(serde_json::json!({"listen":"external"})),
    )
    .unwrap();
    let access = load_access(&profile, &SubjectId::new(b"owner").unwrap())
        .await
        .unwrap();
    let contact = crate::contact::database(&profile).unwrap();
    let mut log = open_access_log(&profile).unwrap();
    writeln!(log, "owned access log").unwrap();
    cache_ocsp(&profile, b"owned cached response").unwrap();
    for relative in [
        "",
        ".volume",
        ".volume/sys",
        "file",
        "ssl",
        "ssl/fullchain.crt",
        ".volume/sys/setting/database",
        ".volume/sys/pkg/database",
        ".volume/sys/access/database",
        ".volume/sys/contact/database",
        "log/access.log",
    ] {
        let metadata = profile.join(relative).metadata().unwrap();
        assert_eq!(
            (metadata.uid(), metadata.gid()),
            (10001, 10002),
            "{relative}"
        );
    }
    for service in ["setting", "pkg", "access", "contact"] {
        for name in ["database-wal", "database-shm", "database-journal"] {
            let path = profile.join(format!(".volume/sys/{service}/{name}"));
            if let Ok(metadata) = path.metadata() {
                assert_eq!(
                    (metadata.uid(), metadata.gid()),
                    (10001, 10002),
                    "{}",
                    path.display()
                );
            }
        }
    }
    for relative in [
        "log",
        "cache",
        "browser-only",
        "browser-only/ssl",
        "log/error.log",
        "cache/catalogue",
        "browser-only/ssl/fullchain.crt",
        "browser-only/ssl/privkey.pem",
    ] {
        let path = account.join(".dhttp").join(relative);
        let metadata = path.metadata().unwrap();
        assert_eq!(
            (metadata.uid(), metadata.gid()),
            (10001, 10002),
            "{relative}"
        );
        if metadata.is_file() {
            assert_eq!(std::fs::read(path).unwrap(), b"retained account data");
        }
    }
    assert_eq!(
        account
            .join(".dhttp/browser-only/ssl/privkey.pem")
            .metadata()
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o400
    );
    let metadata = profile.ocsp_path().metadata().unwrap();
    assert_eq!((metadata.uid(), metadata.gid()), (10001, 10002));
    let metadata = account.join(".dhttp").metadata().unwrap();
    assert_eq!((metadata.uid(), metadata.gid()), (10001, 10002));
    let metadata = outside.metadata().unwrap();
    assert_eq!(
        (metadata.uid(), metadata.gid()),
        (outside_owner.uid(), outside_owner.gid())
    );
    assert_eq!(metadata.permissions().mode() & 0o777, 0o640);
    assert_eq!(std::fs::read(outside).unwrap(), b"untouched");
    drop((config, contact, access, log));
}

#[test]
fn legacy_layout_migration_preserves_wal_data_and_signed_app_layouts() {
    let root = tempfile::tempdir().unwrap();
    let profile = IdentityProfile::try_from(root.path().join("legacy")).unwrap();
    for directory in [
        "libs/std",
        "apps/plain/public/assets",
        "apps/signed/public",
        "logs",
        "db",
    ] {
        std::fs::create_dir_all(profile.join(directory)).unwrap();
    }
    std::fs::write(profile.join("libs/std/notes.wasm"), b"component").unwrap();
    std::fs::write(profile.join("logs/access.log"), b"request log").unwrap();
    std::fs::write(profile.join("apps/plain/public/index.html"), b"page").unwrap();
    std::fs::write(profile.join("apps/plain/public/assets/app.js"), b"script").unwrap();
    std::fs::write(
        profile.join("apps/signed/.webc-proof"),
        b"signed archive proof",
    )
    .unwrap();
    std::fs::write(
        profile.join("apps/signed/public/index.html"),
        b"signed page",
    )
    .unwrap();
    let mut writers = Vec::new();
    for service in ["access", "contact"] {
        let db = rusqlite::Connection::open(profile.join(format!("db/{service}.db"))).unwrap();
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE saved(value TEXT); INSERT INTO saved VALUES('committed WAL');").unwrap();
        writers.push(db);
    }
    initialize_directories(&profile).unwrap();
    assert_eq!(
        std::fs::read(profile.join("lib/std/notes.wasm")).unwrap(),
        b"component"
    );
    assert_eq!(
        std::fs::read(profile.join("log/access.log")).unwrap(),
        b"request log"
    );
    assert_eq!(
        std::fs::read(profile.join("app/plain/index.html")).unwrap(),
        b"page"
    );
    assert_eq!(
        std::fs::read(profile.join("app/plain/assets/app.js")).unwrap(),
        b"script"
    );
    assert!(!profile.join("app/plain/public").exists());
    assert_eq!(
        std::fs::read(profile.join("app/signed/public/index.html")).unwrap(),
        b"signed page"
    );
    for (service, writer) in ["access", "contact"].into_iter().zip(&writers) {
        let migrated =
            rusqlite::Connection::open(profile.join(format!(".volume/sys/{service}/database")))
                .unwrap();
        assert_eq!(
            migrated
                .query_row("SELECT value FROM saved", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "committed WAL"
        );
        writer
            .execute("INSERT INTO saved VALUES('subsequent legacy write')", [])
            .unwrap();
    }
    // Reopening must not overwrite already migrated native databases.
    initialize_directories(&profile).unwrap();
    for service in ["access", "contact"] {
        let migrated =
            rusqlite::Connection::open(profile.join(format!(".volume/sys/{service}/database")))
                .unwrap();
        assert_eq!(
            migrated
                .query_row("SELECT count(*) FROM saved", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert!(profile.join(format!("db/{service}.db")).exists());
    }
}

#[test]
fn conflicting_legacy_directories_and_public_files_preserve_both_versions() {
    let root = tempfile::tempdir().unwrap();
    for legacy in ["libs", "apps", "logs"] {
        let profile = IdentityProfile::try_from(root.path().join(legacy)).unwrap();
        let current = match legacy {
            "libs" => "lib",
            "apps" => "app",
            _ => "log",
        };
        std::fs::create_dir_all(profile.join(legacy)).unwrap();
        std::fs::create_dir(profile.join(current)).unwrap();
        std::fs::write(profile.join(legacy).join("old"), b"old resource").unwrap();
        std::fs::write(profile.join(current).join("new"), b"new resource").unwrap();
        assert!(initialize_directories(&profile).is_err());
        assert_eq!(
            std::fs::read(profile.join(legacy).join("old")).unwrap(),
            b"old resource"
        );
        assert_eq!(
            std::fs::read(profile.join(current).join("new")).unwrap(),
            b"new resource"
        );
    }
    let profile = profile(root.path(), "public-conflict");
    std::fs::create_dir_all(profile.join("app/notes/public")).unwrap();
    std::fs::write(profile.join("app/notes/public/index.html"), b"legacy page").unwrap();
    std::fs::write(profile.join("app/notes/index.html"), b"installed page").unwrap();
    assert!(initialize_directories(&profile).is_err());
    assert_eq!(
        std::fs::read(profile.join("app/notes/public/index.html")).unwrap(),
        b"legacy page"
    );
    assert_eq!(
        std::fs::read(profile.join("app/notes/index.html")).unwrap(),
        b"installed page"
    );
}

#[test]
fn interrupted_package_updates_recover_both_resources_at_the_database_commit_boundary() {
    for author in ["std.dhttp.net", "alice.dhttp.net"] {
        for committed in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let profile = profile(root.path(), "owner");
            load_server_config(&profile).unwrap();
            let db =
                crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
                    .unwrap();
            db.execute("INSERT INTO package(name,author,version,has_lib,has_app) VALUES('notes',?1,'2.0.0',1,1)", [author]).unwrap();
            let facts: String = db
                .query_row(
                    include_str!("../../../src/package/registration-facts.sql"),
                    ["notes"],
                    |r| r.get(0),
                )
                .unwrap();
            if !committed {
                db.execute("UPDATE package SET version='1.0.0' WHERE name='notes'", [])
                    .unwrap();
            }
            let lib = if author == "std.dhttp.net" {
                "lib/std/notes.wasm"
            } else {
                "lib/notes.wasm"
            };
            std::fs::write(profile.join(lib), b"new component").unwrap();
            std::fs::create_dir_all(profile.join("app/notes")).unwrap();
            std::fs::write(profile.join("app/notes/index.html"), b"new page").unwrap();
            std::fs::create_dir_all(profile.join(".volume/notes")).unwrap();
            std::fs::write(profile.join(".volume/notes/database"), b"business data").unwrap();
            let stage = profile.join(".pishoo-interrupted-update");
            std::fs::create_dir_all(stage.join("old-app")).unwrap();
            std::fs::write(stage.join("old-lib"), b"old component").unwrap();
            std::fs::write(stage.join("old-app/index.html"), b"old page").unwrap();
            let journal = serde_json::json!({"id":format!("{author}/notes"),"facts":facts,"resources":[
                {"kind":"lib","old":true,"new":true},{"kind":"app","old":true,"new":true}]});
            std::fs::write(
                stage.join(".transaction.json"),
                serde_json::to_vec(&journal).unwrap(),
            )
            .unwrap();
            initialize_directories(&profile).unwrap();
            assert_eq!(
                std::fs::read(profile.join(lib)).unwrap(),
                if committed {
                    b"new component"
                } else {
                    b"old component"
                }
            );
            assert_eq!(
                std::fs::read(profile.join("app/notes/index.html")).unwrap(),
                if committed { b"new page" } else { b"old page" }
            );
            assert_eq!(
                db.query_row("SELECT version FROM package WHERE name='notes'", [], |r| {
                    r.get::<_, String>(0)
                })
                .unwrap(),
                if committed { "2.0.0" } else { "1.0.0" }
            );
            assert_eq!(
                std::fs::read(profile.join(".volume/notes/database")).unwrap(),
                b"business data"
            );
            assert!(!stage.exists());
            initialize_directories(&profile).unwrap();
        }
    }
}

#[test]
fn interrupted_first_install_removes_only_published_unregistered_resources() {
    for published in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let profile = profile(root.path(), "owner");
        load_server_config(&profile).unwrap();
        let stage = profile.join(".pishoo-interrupted-install");
        std::fs::create_dir_all(stage.join("new-lib")).unwrap();
        std::fs::create_dir_all(stage.join("new-app")).unwrap();
        std::fs::write(stage.join("new-lib/notes.wasm"), b"candidate component").unwrap();
        std::fs::write(stage.join("new-app/index.html"), b"candidate page").unwrap();
        if published {
            std::fs::rename(
                stage.join("new-lib/notes.wasm"),
                profile.join("lib/notes.wasm"),
            )
            .unwrap();
            std::fs::rename(stage.join("new-app"), profile.join("app/notes")).unwrap();
        }
        std::fs::write(profile.join("lib/unrelated.wasm"), b"unrelated component").unwrap();
        let journal = serde_json::json!({"id":"alice.dhttp.net/notes","facts":"uncommitted installation","resources":[
            {"kind":"lib","old":false,"new":true},{"kind":"app","old":false,"new":true}]});
        std::fs::write(
            stage.join(".transaction.json"),
            serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();
        initialize_directories(&profile).unwrap();
        assert!(!profile.join("lib/notes.wasm").exists());
        assert!(!profile.join("app/notes").exists());
        assert!(!stage.exists());
        assert_eq!(
            std::fs::read(profile.join("lib/unrelated.wasm")).unwrap(),
            b"unrelated component"
        );
        let db = crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
        assert_eq!(
            db.query_row("SELECT count(*) FROM package", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}

#[test]
fn malformed_recovery_journals_preserve_installed_resources_for_manual_repair() {
    for journal in [
        serde_json::json!({}),
        serde_json::json!({"id":"../notes","resources":[]}),
        serde_json::json!({"id":"alice.dhttp.net/notes","resources":[]}),
        serde_json::json!({"id":"alice.dhttp.net/notes","resources":[{"kind":"app","old":true,"new":true},{"kind":"lib","old":true,"new":true}]}),
        serde_json::json!({"id":"alice.dhttp.net/notes","resources":[{"kind":"lib","old":"yes","new":true},{"kind":"app","old":true,"new":true}]}),
    ] {
        let root = tempfile::tempdir().unwrap();
        let profile = profile(root.path(), "owner");
        load_server_config(&profile).unwrap();
        std::fs::write(profile.join("lib/notes.wasm"), b"installed component").unwrap();
        let stage = profile.join(".pishoo-invalid-journal");
        std::fs::create_dir(&stage).unwrap();
        let bytes = serde_json::to_vec(&journal).unwrap();
        std::fs::write(stage.join(".transaction.json"), &bytes).unwrap();
        assert!(initialize_directories(&profile).is_err());
        assert_eq!(
            std::fs::read(profile.join("lib/notes.wasm")).unwrap(),
            b"installed component"
        );
        assert_eq!(
            std::fs::read(stage.join(".transaction.json")).unwrap(),
            bytes
        );
    }
}

#[cfg(unix)]
#[test]
fn ownership_traversal_preserves_permissions_and_skips_links_outside_the_identity() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    let root = tempfile::tempdir().unwrap();
    let profile = profile(root.path(), "owner");
    std::fs::create_dir_all(profile.join("app/notes/assets")).unwrap();
    std::fs::write(profile.join("app/notes/assets/app.js"), b"static asset").unwrap();
    std::fs::write(profile.join("lib/std/notes.wasm"), b"component").unwrap();
    std::fs::set_permissions(
        profile.join("lib/std/notes.wasm"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let external = root.path().join("external");
    std::fs::write(&external, b"outside identity").unwrap();
    std::fs::hard_link(&external, profile.join("app/linked-file")).unwrap();
    symlink(&external, profile.join("app/external-link")).unwrap();
    symlink(
        root.path().join("absent-directory"),
        profile.join("lib/dangling-link"),
    )
    .unwrap();
    let before = external.metadata().unwrap();
    let identity = std::fs::File::open(profile.path()).unwrap();
    let owner = identity.metadata().unwrap();
    for _ in 0..2 {
        repair_identity_ownership(&identity, &owner).unwrap();
        let component = profile.join("lib/std/notes.wasm").metadata().unwrap();
        assert_eq!(
            (component.uid(), component.gid()),
            (owner.uid(), owner.gid())
        );
        assert_eq!(component.permissions().mode() & 0o777, 0o600);
        assert_eq!(
            std::fs::read(profile.join("app/notes/assets/app.js")).unwrap(),
            b"static asset"
        );
        assert!(
            profile
                .join("lib/dangling-link")
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let after = external.metadata().unwrap();
        assert_eq!(
            (
                after.ino(),
                after.nlink(),
                after.uid(),
                after.gid(),
                after.mode()
            ),
            (
                before.ino(),
                before.nlink(),
                before.uid(),
                before.gid(),
                before.mode()
            )
        );
        assert_eq!(std::fs::read(&external).unwrap(), b"outside identity");
    }
}
