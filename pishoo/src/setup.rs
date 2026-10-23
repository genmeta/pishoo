//! Read, validate and manage identity service configuration.
use std::{collections::HashSet, net::SocketAddr};

use access_control::{SubjectId, Visitor};
use axum::{
    Json, Router,
    body::Body,
    response::{IntoResponse, Response},
    routing::any,
};
use dhttp_home::identity::IdentityProfile;
use http::{Method, Request, StatusCode, header};
use rusqlite::{Connection, OpenFlags, TransactionBehavior, types::ValueRef};
use serde_json::{Value, json};

use crate::{Error, Result, routes::reserved};

#[derive(Clone, Debug)]
pub(crate) struct ServerConfig {
    pub(crate) listen: String,
    pub(crate) proxy_locations: Vec<ProxyLocation>,
}
#[derive(Debug)]
pub(crate) struct ProxyLocation {
    pub(crate) location: String,
    pub(crate) proxy_pass: http::uri::Parts,
}
impl Clone for ProxyLocation {
    fn clone(&self) -> Self {
        let mut proxy_pass = http::uri::Parts::default();
        proxy_pass.scheme = self.proxy_pass.scheme.clone();
        proxy_pass.authority = self.proxy_pass.authority.clone();
        proxy_pass.path_and_query = self.proxy_pass.path_and_query.clone();
        Self {
            location: self.location.clone(),
            proxy_pass,
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/setup.rs"]
mod tests;

pub(crate) fn load_server_config(profile: &IdentityProfile) -> Result<ServerConfig> {
    initialize_config(profile)
        .and_then(|()| {
            let mut conn = open_config(profile, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            read_server_config(&mut conn)
        })
        .map_err(|error| {
            Error::InvalidConfig(format!(
                "configuration database {}: {error}",
                profile.join(".volume/sys/setting/database").display()
            ))
        })
}

// Runtime scans read one snapshot without startup initialization or integrity checks.
pub(crate) fn read_server_config(conn: &mut Connection) -> Result<ServerConfig> {
    let tx = conn.transaction()?;
    let config = read_config(&tx)?;
    check_proxy_mounts(&tx, &config.proxy_locations)?;
    tx.commit()?;
    Ok(config)
}

fn initialize_legacy_config(profile: &IdentityProfile) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    std::fs::create_dir_all(profile.path())?;
    if !profile.path().symlink_metadata()?.file_type().is_dir() {
        return Err(Error::InvalidConfig(
            "identity must be a real directory".into(),
        ));
    }
    for name in [
        ".volume",
        ".volume/sys",
        ".volume/sys/setting",
        ".volume/sys/lib",
        ".volume/sys/app",
        ".volume/sys/access",
        ".volume/sys/contact",
    ] {
        let path = profile.join(name);
        match std::fs::DirBuilder::new()
            .recursive(false)
            .mode(0o700)
            .create(&path)
        {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        if !path.symlink_metadata()?.file_type().is_dir() {
            return Err(Error::InvalidConfig(
                "volume must contain real directories".into(),
            ));
        }
    }
    let path = profile.join(".volume/sys/setting/database");
    if !path.try_exists()? {
        let old = profile.config_db_path();
        let staging = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
        if old.try_exists()? {
            if !old.symlink_metadata()?.file_type().is_file() {
                return Err(Error::InvalidConfig(
                    "legacy configuration must be a regular file".into(),
                ));
            }
            let source = Connection::open_with_flags(old, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            source.backup(rusqlite::DatabaseName::Main, staging.path(), None)?;
        }
        staging.as_file().sync_all()?;
        match staging.persist_noclobber(&path) {
            Ok(_) => {}
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.error.into()),
        }
    }
    for name in ["lib", "app", "access"] {
        let path = profile.join(format!(".volume/sys/{name}/database"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
        {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    let mut conn = open_legacy_config(profile, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    let integrity: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    if integrity != "ok" {
        return Err(Error::InvalidConfig(integrity));
    }
    let mut version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    let staged_legacy: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('lib','main') WHERE name='id')",
        [],
        |r| r.get(0),
    )?;
    if staged_legacy {
        version = 1;
    }
    if version == 2 {
        for (schema, table) in [
            ("main", "setting"),
            ("lib_registry", "lib"),
            ("app_registry", "app"),
            ("acl", "lib_capability_operation"),
        ] {
            let exists: bool = conn.query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM {schema}.sqlite_master WHERE name=?1)"),
                [table],
                |r| r.get(0),
            )?;
            if !exists {
                return Err(Error::InvalidConfig(
                    "incomplete split database schema".into(),
                ));
            }
        }
        return Ok(());
    }
    if version != 0 && version != 1 {
        return Err(Error::InvalidConfig(
            "unsupported configuration schema".into(),
        ));
    }
    if version == 0
        && conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM main.sqlite_master WHERE name NOT LIKE 'sqlite_%')",
            [],
            |r| r.get::<_, bool>(0),
        )?
    {
        return Err(Error::InvalidConfig(
            "unrecognized configuration database".into(),
        ));
    }
    let legacy = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM main.sqlite_master WHERE name='settings')",
        [],
        |r| r.get::<_, bool>(0),
    )?;
    // Read and validate the old configuration before modifying anything.
    let kv: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM main.sqlite_master WHERE name='setting')",
        [],
        |r| r.get(0),
    )?;
    let listen = if legacy || kv {
        read_config(&conn)?.listen
    } else {
        "off".into()
    };
    for schema in ["main", "lib_registry", "app_registry", "acl"] {
        conn.execute_batch(&format!(
            "PRAGMA {schema}.journal_mode=DELETE; PRAGMA {schema}.synchronous=FULL;"
        ))?;
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if staged_legacy {
        for (singular, plural) in [
            ("lib", "libs"),
            ("app", "apps"),
            ("app_dependency", "app_dependencies"),
            ("lib_dependency", "lib_dependencies"),
            ("lib_capability", "lib_capabilities"),
            ("lib_capability_operation", "lib_capability_operations"),
        ] {
            if tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM main.sqlite_master WHERE name=?1)",
                [singular],
                |r| r.get::<_, bool>(0),
            )? {
                tx.execute_batch(&format!("ALTER TABLE main.{singular} RENAME TO {plural}"))?;
            }
        }
    }
    tx.execute_batch("CREATE TABLE IF NOT EXISTS setting(key TEXT PRIMARY KEY NOT NULL,value TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS proxy_location(location TEXT PRIMARY KEY NOT NULL,proxy_pass TEXT NOT NULL);")?;
    if listen != "off" {
        tx.execute("INSERT INTO setting VALUES('listen',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [&listen])?;
    }
    let old_proxy: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM main.sqlite_master WHERE name='proxy_locations')",
        [],
        |r| r.get(0),
    )?;
    if old_proxy {
        tx.execute_batch("INSERT INTO proxy_location SELECT * FROM proxy_locations;")?;
    }
    tx.execute_batch(include_str!("package/sql/schema-v2.sql"))?;
    for (old, schema, table) in [
        ("libs", "lib_registry", "lib"),
        ("apps", "app_registry", "app"),
    ] {
        let columns = tx
            .prepare(&format!("PRAGMA main.table_info({old})"))?
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if columns.is_empty() {
            continue;
        }
        if columns.iter().any(|c| c == "package_proof")
            && tx.query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM main.{old} WHERE package_proof IS NOT NULL)"),
                [],
                |r| r.get::<_, bool>(0),
            )?
        {
            return Err(Error::InvalidConfig(
                "unpublished proof layout needs explicit recovery; original data preserved".into(),
            ));
        }
        let author = if columns.iter().any(|c| c == "author") {
            "author"
        } else {
            "CASE WHEN instr(id,'/')=0 THEN NULL ELSE substr(id,1,instr(id,'/')-1) END"
        };
        let fields = [
            "title",
            "description",
            "license",
            "repository",
            "installed_at",
            "upgraded_at",
        ]
        .map(|field| {
            if columns.iter().any(|c| c == field) {
                field
            } else {
                "NULL"
            }
        })
        .join(",");
        tx.execute_batch(&format!("INSERT INTO {schema}.{table}(name,author,version,title,description,license,repository,installed_at,upgraded_at) SELECT name,{author},version,{fields} FROM main.{old}"))?;
    }
    for (old, schema, table, modern, legacy_sql) in [
        (
            "app_dependencies",
            "app_registry",
            "app_dependency",
            "app_name",
            "SELECT a.name,l.name,d.version FROM main.app_dependencies d JOIN main.apps a ON a.id=d.app_id JOIN main.libs l ON l.id=d.lib_id",
        ),
        (
            "lib_dependencies",
            "lib_registry",
            "lib_dependency",
            "lib_name",
            "SELECT l.name,t.name,d.version FROM main.lib_dependencies d JOIN main.libs l ON l.id=d.lib_id JOIN main.libs t ON t.id=d.dependency_id",
        ),
        (
            "lib_capabilities",
            "acl",
            "lib_capability",
            "lib_name",
            "SELECT l.name,c.capability_id,c.title FROM main.lib_capabilities c JOIN main.libs l ON l.id=c.lib_id",
        ),
        (
            "lib_capability_operations",
            "acl",
            "lib_capability_operation",
            "lib_name",
            "SELECT l.name,c.capability_id,c.method,c.path FROM main.lib_capability_operations c JOIN main.libs l ON l.id=c.lib_id",
        ),
    ] {
        let columns = tx
            .prepare(&format!("PRAGMA main.table_info({old})"))?
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if columns.is_empty() {
            continue;
        }
        let select = if columns.iter().any(|c| c == modern) {
            format!("SELECT * FROM main.{old}")
        } else {
            legacy_sql.to_owned()
        };
        tx.execute_batch(&format!("INSERT INTO {schema}.{table} {select}"))?;
        let copied: bool = tx.query_row(
            &format!(
                "SELECT (SELECT count(*) FROM {schema}.{table})=(SELECT count(*) FROM main.{old})"
            ),
            [],
            |r| r.get(0),
        )?;
        if !copied {
            return Err(Error::InvalidConfig(
                "legacy references could not be migrated".into(),
            ));
        }
    }
    tx.execute_batch("UPDATE lib_capability_operation SET path='/'||lib_name||substr(path,5) WHERE substr(path,1,5)='/std/' AND lib_name IN (SELECT name FROM lib WHERE author IS NULL OR author<>'std.dhttp.net');")?;
    for schema in ["lib_registry", "app_registry", "acl"] {
        let broken = tx
            .prepare(&format!("PRAGMA {schema}.foreign_key_check"))?
            .exists([])?;
        if broken {
            return Err(Error::InvalidConfig("broken migrated foreign keys".into()));
        }
    }
    let broken:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM app_dependency d LEFT JOIN lib l ON l.name=d.lib_name WHERE l.name IS NULL OR l.version<>d.version) OR EXISTS(SELECT 1 FROM lib_dependency d JOIN lib l ON l.name=d.dependency_name WHERE l.version<>d.version) OR EXISTS(SELECT 1 FROM lib_capability c LEFT JOIN lib l ON l.name=c.lib_name WHERE l.name IS NULL)",[],|r|r.get(0))?;
    if broken {
        return Err(Error::InvalidConfig(
            "invalid migrated dependencies or capability owners".into(),
        ));
    }
    // The original db/config.db is the backup. Drop only the migrated snapshot's tables.
    tx.execute_batch("DROP TABLE IF EXISTS main.lib_capability_operations; DROP TABLE IF EXISTS main.lib_capabilities;
        DROP TABLE IF EXISTS main.app_dependencies; DROP TABLE IF EXISTS main.lib_dependencies;
        DROP TABLE IF EXISTS main.apps; DROP TABLE IF EXISTS main.libs;
        DROP TABLE IF EXISTS main.proxy_locations; DROP TABLE IF EXISTS main.settings; PRAGMA main.user_version=2;")?;
    tx.commit()?;
    Ok(())
}

fn open_legacy_config(profile: &IdentityProfile, flags: OpenFlags) -> Result<Connection> {
    let current = profile.join(".volume/sys/setting/database");
    let db = if !current.try_exists()? && flags.contains(OpenFlags::SQLITE_OPEN_READ_ONLY) {
        profile.config_db_path()
    } else {
        current
    };
    if !db.symlink_metadata()?.file_type().is_file() {
        return Err(Error::InvalidConfig(
            "configuration must be a regular file".into(),
        ));
    }
    let conn = Connection::open_with_flags(&db, flags)?;
    conn.busy_timeout(std::time::Duration::from_secs(3))?;
    conn.pragma_update(None, "foreign_keys", true)?;
    if db == profile.join(".volume/sys/setting/database") {
        for (schema, name) in [
            ("lib_registry", "lib"),
            ("app_registry", "app"),
            ("acl", "access"),
        ] {
            let path = profile.join(format!(".volume/sys/{name}/database"));
            if !path.try_exists()? && flags.contains(OpenFlags::SQLITE_OPEN_READ_ONLY) {
                let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
                if version < 2 {
                    continue;
                }
            }
            if !path.symlink_metadata()?.file_type().is_file() {
                return Err(Error::InvalidConfig(
                    "attached database must be a regular file".into(),
                ));
            }
            // URI mode prevents reads from creating a missing attached database.
            let mode = if flags.contains(OpenFlags::SQLITE_OPEN_READ_ONLY) {
                "ro"
            } else {
                "rw"
            };
            let uri = format!(
                "file:{}?mode={mode}",
                url::Url::from_file_path(path)
                    .map_err(|_| Error::InvalidConfig("invalid database path".into()))?
                    .path()
            );
            conn.execute(&format!("ATTACH DATABASE ?1 AS {schema}"), [uri])?;
        }
    }
    Ok(conn)
}

fn initialize_config(profile: &IdentityProfile) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    std::fs::create_dir_all(profile.path())?;
    if !profile.path().symlink_metadata()?.file_type().is_dir() {
        return Err(Error::InvalidConfig(
            "identity must be a real directory".into(),
        ));
    }
    let existing = if profile.join(".volume/sys/setting/database").try_exists()? {
        profile.join(".volume/sys/setting/database")
    } else {
        profile.config_db_path()
    };
    if existing.try_exists()? {
        if !existing.symlink_metadata()?.file_type().is_file() {
            return Err(Error::InvalidConfig(
                "configuration must be a regular file".into(),
            ));
        }
        let previous = Connection::open_with_flags(&existing, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let version: i64 = previous.pragma_query_value(None, "user_version", |r| r.get(0))?;
        let occupied: bool = previous.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name NOT LIKE 'sqlite_%')",
            [],
            |r| r.get(0),
        )?;
        drop(previous);
        if version == 1 {
            initialize_legacy_config(profile)?;
        } else if version == 0 && occupied {
            return Err(Error::InvalidConfig(
                "unrecognized configuration database; existing data preserved".into(),
            ));
        }
    }
    for name in [
        ".volume",
        ".volume/sys",
        ".volume/sys/setting",
        ".volume/sys/pkg",
        ".volume/sys/access",
        ".volume/sys/contact",
    ] {
        let path = profile.join(name);
        match std::fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
        if !path.symlink_metadata()?.file_type().is_dir() {
            return Err(Error::InvalidConfig(
                "volume must contain real directories".into(),
            ));
        }
    }
    for name in ["setting", "pkg", "access"] {
        let path = profile.join(format!(".volume/sys/{name}/database"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
        {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
    }
    let mut conn = open_config(profile, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    let check: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    if check != "ok" {
        return Err(Error::InvalidConfig(check));
    }
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version == 3 {
        for (schema, table) in [
            ("main", "setting"),
            ("main", "proxy_location"),
            ("pkg_registry", "package"),
            ("pkg_registry", "package_dependency"),
            ("acl", "lib_capability"),
            ("acl", "lib_capability_operation"),
        ] {
            if !conn.query_row(&format!("SELECT EXISTS(SELECT 1 FROM {schema}.sqlite_master WHERE type='table' AND name=?1)"),[table],|r|r.get::<_,bool>(0))? {
                return Err(Error::InvalidConfig("incomplete package registry schema".into()));
            }
        }
        return Ok(());
    }
    if !matches!(version, 0 | 2) {
        return Err(Error::InvalidConfig(
            "unsupported configuration schema; existing data preserved".into(),
        ));
    }
    for schema in ["main", "pkg_registry", "acl"] {
        conn.execute_batch(&format!(
            "PRAGMA {schema}.journal_mode=DELETE; PRAGMA {schema}.synchronous=FULL;"
        ))?;
    }
    // Baseline v2 identities retain their old databases. Merge only compatible
    // same-name resources; ambiguous author/version pairs require explicit resolution.
    if version == 2 {
        for (schema, name) in [("old_lib", "lib"), ("old_app", "app")] {
            let path = profile.join(format!(".volume/sys/{name}/database"));
            if !path.symlink_metadata()?.file_type().is_file() {
                return Err(Error::InvalidConfig(
                    "legacy package database must be a regular file".into(),
                ));
            }
            let uri = format!(
                "file:{}?mode=ro",
                url::Url::from_file_path(path)
                    .map_err(|_| Error::InvalidConfig("invalid path".into()))?
                    .path()
            );
            conn.execute(&format!("ATTACH DATABASE ?1 AS {schema}"), [uri])?;
        }
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch("CREATE TABLE IF NOT EXISTS setting(key TEXT PRIMARY KEY NOT NULL,value TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS proxy_location(location TEXT PRIMARY KEY NOT NULL,proxy_pass TEXT NOT NULL);")?;
    tx.execute_batch(include_str!("package/sql/schema-v3.sql"))?;
    if version == 2 {
        let conflict: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM old_lib.lib l JOIN old_app.app a USING(name) WHERE l.author IS NOT a.author OR l.version<>a.version)", [], |r| r.get(0))?;
        if conflict {
            return Err(Error::InvalidConfig(
                "same-name App/Lib author or version differs; existing packages preserved".into(),
            ));
        }
        let dependency_conflict: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM old_lib.lib_dependency l JOIN old_app.app_dependency a ON l.lib_name=a.app_name AND l.dependency_name=a.lib_name WHERE l.lib_name<>l.dependency_name AND l.version<>a.version)",[],|r|r.get(0))?;
        if dependency_conflict {
            return Err(Error::InvalidConfig(
                "conflicting exact dependencies in same-name App/Lib; existing data preserved"
                    .into(),
            ));
        }
        tx.execute_batch("INSERT INTO pkg_registry.package SELECT name,author,version,title,description,license,repository,installed_at,upgraded_at,1,0 FROM old_lib.lib;
            INSERT INTO pkg_registry.package SELECT name,author,version,title,description,license,repository,installed_at,upgraded_at,0,1 FROM old_app.app WHERE true
            ON CONFLICT(name) DO UPDATE SET has_app=1;
            INSERT INTO pkg_registry.package_dependency SELECT lib_name,dependency_name,version FROM old_lib.lib_dependency WHERE lib_name<>dependency_name;
            INSERT INTO pkg_registry.package_dependency SELECT app_name,lib_name,version FROM old_app.app_dependency WHERE app_name<>lib_name AND true
            ON CONFLICT(package_name,dependency_name) DO NOTHING;")?;
        let broken: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM pkg_registry.package_dependency d LEFT JOIN pkg_registry.package p ON p.name=d.dependency_name WHERE p.name IS NULL OR p.version<>d.version)", [], |r|r.get(0))?;
        if broken {
            return Err(Error::InvalidConfig(
                "broken package dependency; existing data preserved".into(),
            ));
        }
    }
    tx.pragma_update(None, "user_version", 3)?;
    tx.commit()?;
    inherit_config_files(profile)?;
    Ok(())
}

pub(crate) fn open_config(profile: &IdentityProfile, flags: OpenFlags) -> Result<Connection> {
    let current = profile.join(".volume/sys/setting/database");
    let db = if !current.try_exists()? && flags.contains(OpenFlags::SQLITE_OPEN_READ_ONLY) {
        profile.config_db_path()
    } else {
        current
    };
    if !db.symlink_metadata()?.file_type().is_file() {
        return Err(Error::InvalidConfig(
            "configuration must be a regular file".into(),
        ));
    }
    let conn = Connection::open_with_flags(&db, flags)?;
    conn.busy_timeout(std::time::Duration::from_secs(3))?;
    conn.pragma_update(None, "foreign_keys", true)?;
    if db == profile.join(".volume/sys/setting/database") {
        for (schema, name) in [("pkg_registry", "pkg"), ("acl", "access")] {
            let path = profile.join(format!(".volume/sys/{name}/database"));
            if !path.try_exists()? && flags.contains(OpenFlags::SQLITE_OPEN_READ_ONLY) {
                let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
                if version < 3 {
                    continue;
                }
            }
            if !path.symlink_metadata()?.file_type().is_file() {
                return Err(Error::InvalidConfig(
                    "attached database must be a regular file".into(),
                ));
            }
            // URI mode prevents reads from creating a missing attached database.
            let mode = if flags.contains(OpenFlags::SQLITE_OPEN_READ_ONLY) {
                "ro"
            } else {
                "rw"
            };
            let uri = format!(
                "file:{}?mode={mode}",
                url::Url::from_file_path(path)
                    .map_err(|_| Error::InvalidConfig("invalid database path".into()))?
                    .path()
            );
            conn.execute(&format!("ATTACH DATABASE ?1 AS {schema}"), [uri])?;
        }
    }
    if conn.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_list WHERE schema='pkg_registry' AND name='package')", [], |r|r.get::<_,bool>(0))? {
        conn.execute_batch("CREATE TEMP VIEW lib AS SELECT * FROM pkg_registry.package WHERE has_lib=1;
            CREATE TEMP VIEW app AS SELECT * FROM pkg_registry.package WHERE has_app=1;")?;
    }
    Ok(conn)
}

fn inherit_config_files(profile: &IdentityProfile) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    use rustix::fs::{Mode, OFlags};
    if !rustix::process::geteuid().is_root() {
        return Ok(());
    }
    let owner = profile.path().symlink_metadata()?;
    for name in ["setting", "pkg", "access"] {
        let directory = std::fs::File::from(
            rustix::fs::open(
                profile.join(format!(".volume/sys/{name}")),
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(std::io::Error::from)?,
        );
        for file in [
            "database",
            "database-wal",
            "database-shm",
            "database-journal",
        ] {
            let file = match rustix::fs::openat(
                &directory,
                file,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(file) => std::fs::File::from(file),
                Err(rustix::io::Errno::NOENT) => continue,
                Err(error) => return Err(std::io::Error::from(error).into()),
            };
            let metadata = file.metadata()?;
            if !metadata.is_file() || metadata.nlink() != 1 {
                return Err(Error::InvalidConfig(
                    "database files must be single-link regular files".into(),
                ));
            }
            rustix::fs::fchown(
                &file,
                Some(rustix::fs::Uid::from_raw(owner.uid())),
                Some(rustix::fs::Gid::from_raw(owner.gid())),
            )
            .map_err(std::io::Error::from)?;
            file.set_permissions(metadata.permissions())?;
        }
    }
    Ok(())
}

fn read_config(conn: &Connection) -> Result<ServerConfig> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if !matches!(version, 1..=3) {
        return Err(Error::InvalidConfig(format!(
            "unsupported schema {version}"
        )));
    }
    let kv: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM main.sqlite_master WHERE name='setting')",
        [],
        |r| r.get(0),
    )?;
    if kv {
        use rusqlite::OptionalExtension;
        let listen: Option<String> = conn
            .query_row("SELECT value FROM setting WHERE key='listen'", [], |r| {
                r.get(0)
            })
            .optional()?;
        let listen = listen.unwrap_or_else(|| "off".into());
        if listen != "off" && !valid_listen(&listen) {
            return Err(Error::InvalidConfig("invalid listen mode".into()));
        }
        if listen == "off"
            && conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM setting WHERE key='listen')",
                [],
                |r| r.get::<_, bool>(0),
            )?
        {
            return Err(Error::InvalidConfig(
                "disabled listening must have no KV record".into(),
            ));
        }
        let values = conn
            .prepare("SELECT location,proxy_pass FROM proxy_location ORDER BY location")?
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        return Ok(ServerConfig {
            listen,
            proxy_locations: parse_proxies(values)?,
        });
    }
    let has_enabled = has_enabled_column(conn)?;
    let mut stmt = conn.prepare(if has_enabled {
        "SELECT listen, enabled FROM settings"
    } else {
        "SELECT listen FROM settings"
    })?;
    let mut rows = stmt.query([])?;
    let Some(row) = rows.next()? else {
        return Err(Error::InvalidConfig(
            "settings must contain exactly one row".into(),
        ));
    };
    // Integers and enabled are accepted only while reading legacy v1 databases.
    let mut listen = match row.get_ref(0)? {
        ValueRef::Text(value) => std::str::from_utf8(value)
            .ok()
            .filter(|value| *value == "off" || valid_listen(value))
            .map(str::to_owned)
            .ok_or_else(|| Error::InvalidConfig("invalid listen mode".into()))?,
        ValueRef::Integer(value @ 0..=3) => {
            ["off", "internal", "external", "all"][value as usize].to_owned()
        }
        _ => return Err(Error::InvalidConfig("invalid listen mode".into())),
    };
    if has_enabled {
        match row.get_ref(1)? {
            ValueRef::Integer(0) => listen = "off".into(),
            ValueRef::Integer(1) => {}
            _ => {
                return Err(Error::InvalidConfig(
                    "enabled must be an integer 0 or 1".into(),
                ));
            }
        }
    }
    if rows.next()?.is_some() {
        return Err(Error::InvalidConfig(
            "settings must contain exactly one row".into(),
        ));
    }
    drop(rows);
    drop(stmt);
    let mut stmt =
        conn.prepare("SELECT location, proxy_pass FROM proxy_locations ORDER BY location")?;
    let values = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let proxy_locations = parse_proxies(values)?;
    Ok(ServerConfig {
        listen,
        proxy_locations,
    })
}

// /std is reserved; App and ordinary Lib mounts are checked independently.
fn check_proxy_mounts(conn: &Connection, proxies: &[ProxyLocation]) -> Result<()> {
    if proxies.iter().any(|proxy| {
        crate::routes::reserved(proxy.location.strip_prefix("= ").unwrap_or(&proxy.location))
    }) {
        return Err(Error::InvalidConfig("proxy cannot occupy /std".into()));
    }
    let has_apps: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_list WHERE type IN ('table','view') AND name='app')",
        [],
        |row| row.get(0),
    )?;
    if has_apps {
        let mut names = conn.prepare("SELECT name FROM app ORDER BY name")?;
        for name in names.query_map([], |row| row.get::<_, String>(0))? {
            let mount = format!("/app/{}", name?);
            if let Some(proxy) = proxies.iter().find(|proxy| {
                let path = proxy.location.strip_prefix("= ").unwrap_or(&proxy.location);
                path == mount || path.starts_with(&format!("{mount}/"))
            }) {
                return Err(Error::InvalidConfig(format!(
                    "proxy {} conflicts with installed App {mount}",
                    proxy.location
                )));
            }
        }
    }
    let has_libs: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_list WHERE type IN ('table','view') AND name='lib')",
        [],
        |row| row.get(0),
    )?;
    if has_libs {
        let has_author: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('lib') WHERE name='author')",
            [],
            |row| row.get(0),
        )?;
        let mut names = conn.prepare(if has_author {
            "SELECT name FROM lib WHERE author IS NULL OR author<>'std.dhttp.net' ORDER BY name"
        } else {
            "SELECT name FROM lib WHERE substr(id,1,14)<>'std.dhttp.net/' ORDER BY name"
        })?;
        for name in names.query_map([], |row| row.get::<_, String>(0))? {
            let mount = format!("/{}", name?);
            if let Some(proxy) = proxies.iter().find(|proxy| {
                let path = proxy.location.strip_prefix("= ").unwrap_or(&proxy.location);
                path == mount || path.starts_with(&format!("{mount}/"))
            }) {
                return Err(Error::InvalidConfig(format!(
                    "proxy {} conflicts with installed Lib {mount}",
                    proxy.location
                )));
            }
        }
    }
    Ok(())
}

fn has_enabled_column(conn: &Connection) -> Result<bool> {
    Ok(conn
        .prepare("PRAGMA table_info(settings)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<Vec<_>, _>>()?
        .iter()
        .any(|column| column == "enabled"))
}

// Called only inside an IMMEDIATE transaction. Validate before modifying legacy v1.
fn upgrade_settings(conn: &Connection) -> Result<()> {
    let config = read_config(conn)?;
    check_proxy_mounts(conn, &config.proxy_locations)
}

fn valid_listen(value: &str) -> bool {
    matches!(value, "loopback" | "internal" | "external" | "all")
}

fn parse_proxies(values: Vec<(String, String)>) -> Result<Vec<ProxyLocation>> {
    let mut proxy_locations = Vec::new();
    let mut seen = HashSet::new();
    for (location, upstream) in values {
        let path = location.strip_prefix("= ").unwrap_or(&location);
        if !valid_path(path) || !seen.insert(location.clone()) || reserved(path) {
            return Err(Error::InvalidConfig(format!(
                "invalid or reserved proxy location: {location}"
            )));
        }
        let bare_address = upstream.parse::<SocketAddr>().ok();
        let uri = bare_address
            .map(|address| format!("http://{address}"))
            .unwrap_or_else(|| upstream.clone());
        let proxy_pass: http::Uri = uri
            .parse()
            .map_err(|_| Error::InvalidConfig("invalid proxy URI".into()))?;
        let address = proxy_pass
            .authority()
            .and_then(|authority| authority.as_str().parse::<SocketAddr>().ok());
        if proxy_pass.scheme_str() != Some("http")
            || address.is_none_or(|address| !address.ip().is_loopback() || address.port() == 0)
            || upstream.contains(['?', '#', '$'])
        {
            return Err(Error::InvalidConfig(
                "proxy_pass must identify a local HTTP/TCP endpoint".into(),
            ));
        }
        let mut proxy_pass = proxy_pass.into_parts();
        if bare_address.is_some()
            || uri
                .split_once("://")
                .is_some_and(|(_, rest)| !rest.contains('/'))
        {
            proxy_pass.path_and_query = None;
        }
        proxy_locations.push(ProxyLocation {
            location,
            proxy_pass,
        });
    }
    proxy_locations.sort_by(|a, b| a.location.cmp(&b.location));
    Ok(proxy_locations)
}

fn valid_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.contains("//")
        && !path.contains(['?', '#', '%', '\\'])
        && !path.split('/').any(|p| p == "." || p == "..")
        && !path.chars().any(|c| c.is_whitespace() || c.is_control())
}

pub(crate) fn config_router(profile: IdentityProfile, endpoint: dhttp::Endpoint) -> Router {
    let handler = move |request: Request<Body>| {
        let (profile, endpoint) = (profile.clone(), endpoint.clone());
        async move {
            let mut response = config_request(profile, endpoint, request)
                .await
                .unwrap_or_else(|error| {
                    let status = match &error {
                        Error::Io(io) if io.kind() == std::io::ErrorKind::InvalidInput => {
                            StatusCode::CONFLICT
                        }
                        _ => error.status(),
                    };
                    if status.is_server_error() {
                        tracing::error!(%error, "configuration API failed");
                        (status, "configuration storage failed").into_response()
                    } else {
                        (status, error.to_string()).into_response()
                    }
                });
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
            response
                .headers_mut()
                .insert("supported-versions", "v1".parse().unwrap());
            response
        }
    };
    Router::new()
        .route("/std/sys/listen", any(handler.clone()))
        .route("/std/sys/mount", any(handler))
}

async fn config_request(
    profile: IdentityProfile,
    endpoint: dhttp::Endpoint,
    request: Request<Body>,
) -> Result<Response> {
    // authorize removes supplied Visitors and constructs this one from the TLS peer.
    let visitor = request.extensions().get::<Visitor>().ok_or(Error::Denied)?;
    let local = endpoint.local_authority()?;
    let ski = dhttp_home::certificate::extract_dhttp_subject_key_identifier(local.certificates())
        .map_err(|_| Error::Denied)?;
    let subject =
        SubjectId::new(ski.owner_hash().as_str().as_bytes()).map_err(|_| Error::Denied)?;
    if visitor.name() != endpoint.name() || visitor.subject_id() != &subject {
        return Err(Error::Denied);
    }

    if request.headers().contains_key("accept-versions") {
        let mut supported = false;
        for versions in request.headers().get_all("accept-versions") {
            let versions = versions
                .to_str()
                .map_err(|_| Error::BadRequest("invalid Accept-Versions".into()))?;
            supported |= versions.split(',').any(|version| version.trim() == "v1");
        }
        if !supported {
            return Ok(StatusCode::HTTP_VERSION_NOT_SUPPORTED.into_response());
        }
    }

    let method = request.method().clone();
    let uri = request.uri().clone();
    if !matches!(method, Method::GET | Method::PATCH | Method::DELETE) {
        return Ok((
            StatusCode::METHOD_NOT_ALLOWED,
            [(header::ALLOW, "GET, PATCH, DELETE")],
        )
            .into_response());
    }
    let reading = method == Method::GET || method == Method::DELETE;
    if !reading
        && !request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
    {
        return Ok(StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response());
    }
    let bytes = match axum::body::to_bytes(request.into_body(), if reading { 0 } else { 64 * 1024 })
        .await
    {
        Ok(bytes) => bytes,
        Err(error) => {
            use std::error::Error as _;
            if error
                .source()
                .is_some_and(|source| source.is::<http_body_util::LengthLimitError>())
            {
                return Ok(if reading {
                    StatusCode::BAD_REQUEST
                } else {
                    StatusCode::PAYLOAD_TOO_LARGE
                }
                .into_response());
            }
            return Err(Error::BadRequest("failed to read request body".into()));
        }
    };
    let payload = if reading {
        None
    } else {
        Some(
            serde_json::from_slice::<Value>(&bytes)
                .map_err(|_| Error::BadRequest("invalid JSON".into()))?,
        )
    };
    let deleting = method == Method::DELETE;
    let value =
        tokio::task::spawn_blocking(move || config_database(&profile, &method, &uri, payload))
            .await??;
    Ok(if deleting {
        StatusCode::NO_CONTENT.into_response()
    } else {
        Json(value).into_response()
    })
}

pub(crate) fn config_database(
    profile: &IdentityProfile,
    method: &Method,
    uri: &http::Uri,
    payload: Option<Value>,
) -> Result<Value> {
    if !matches!(uri.path(), "/std/sys/listen" | "/std/sys/mount") {
        return Err(Error::RouteNotFound);
    }
    let deletion = if let Some(query) = uri.query() {
        if uri.path() != "/std/sys/mount" || *method != Method::DELETE {
            return Err(Error::BadRequest("unsupported query parameters".into()));
        }
        let params = form_urlencoded::parse(query.as_bytes()).collect::<Vec<_>>();
        if params.len() != 2
            || params.iter().filter(|(key, _)| key == "location").count() != 1
            || params.iter().filter(|(key, _)| key == "proxy_pass").count() != 1
        {
            return Err(Error::BadRequest(
                "unmount requires exactly location and proxy_pass".into(),
            ));
        }
        let location = params
            .iter()
            .find(|(key, _)| key == "location")
            .unwrap()
            .1
            .to_string();
        let upstream = params
            .iter()
            .find(|(key, _)| key == "proxy_pass")
            .unwrap()
            .1
            .to_string();
        Some(parse_proxy_json(&json!([{"location":location,"proxy_pass":upstream}]))?.remove(0))
    } else {
        None
    };
    if uri.path() == "/std/sys/mount" && *method == Method::DELETE && deletion.is_none() {
        return Err(Error::BadRequest(
            "unmount requires location and proxy_pass".into(),
        ));
    }
    let config = if *method == Method::GET {
        if payload.is_some() {
            return Err(Error::BadRequest("GET does not accept a body".into()));
        }
        let mut conn = open_config(profile, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        read_server_config(&mut conn)?
    } else {
        let proxy = match method {
            &Method::PATCH if uri.path() == "/std/sys/listen" => {
                validate_listen_patch(
                    payload
                        .as_ref()
                        .ok_or_else(|| Error::BadRequest("missing configuration".into()))?,
                )?;
                None
            }
            &Method::PATCH => Some(
                parse_proxy_json(&Value::Array(vec![
                    payload
                        .clone()
                        .ok_or_else(|| Error::BadRequest("missing configuration".into()))?,
                ]))?
                .remove(0),
            ),
            &Method::DELETE if payload.is_none() => None,
            _ => return Err(Error::MethodNotAllowed),
        };
        initialize_config(profile)?;
        let mut conn = open_config(profile, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        upgrade_settings(&tx)?;
        if *method == Method::DELETE {
            if let Some(rule) = deletion {
                let current = read_config(&tx)?;
                if let Some(existing) = current
                    .proxy_locations
                    .iter()
                    .find(|existing| existing.location == rule.location)
                {
                    if upstream_text(existing) != upstream_text(&rule) {
                        return Err(Error::Io(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "mount upstream changed",
                        )));
                    }
                    tx.execute(
                        "DELETE FROM proxy_location WHERE location=?1",
                        [&rule.location],
                    )?;
                }
            } else {
                tx.execute("DELETE FROM setting WHERE key='listen'", [])?;
            }
        } else if let Some(rule) = proxy {
            tx.execute(
                "DELETE FROM proxy_location WHERE location=?1",
                [&rule.location],
            )?;
            tx.execute(
                "INSERT INTO proxy_location(location,proxy_pass) VALUES(?1,?2)",
                rusqlite::params![rule.location, upstream_text(&rule)],
            )?;
        } else if payload.as_ref().unwrap()["listen"].as_str() == Some("off") {
            tx.execute("DELETE FROM setting WHERE key='listen'", [])?;
        } else {
            tx.execute("INSERT INTO setting(key,value) VALUES('listen',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [payload.as_ref().unwrap()["listen"].as_str().unwrap()])?;
        }
        let config = read_config(&tx)?;
        check_proxy_mounts(&tx, &config.proxy_locations)?;
        tx.commit()?;
        inherit_config_files(profile)?;
        config
    };
    if *method == Method::DELETE {
        return Ok(Value::Null);
    }
    let forwards = config
        .proxy_locations
        .iter()
        .map(|proxy| json!({"location":proxy.location,"proxy_pass":upstream_text(proxy)}))
        .collect::<Vec<_>>();
    Ok(if uri.path() == "/std/sys/listen" {
        json!({"listen":config.listen})
    } else {
        json!(forwards)
    })
}

fn validate_listen_patch(payload: &Value) -> Result<()> {
    let object = payload
        .as_object()
        .filter(|object| object.len() == 1)
        .ok_or_else(|| {
            Error::BadRequest("listen PATCH requires exactly one listen field".into())
        })?;
    for (key, value) in object {
        let valid = match key.as_str() {
            "listen" => value
                .as_str()
                .is_some_and(|mode| matches!(mode, "internal" | "external" | "all" | "off")),
            _ => false,
        };
        if !valid {
            return Err(Error::BadRequest(format!("invalid listen field: {key}")));
        }
    }
    Ok(())
}

fn parse_proxy_json(payload: &Value) -> Result<Vec<ProxyLocation>> {
    let values = payload
        .as_array()
        .ok_or_else(|| Error::BadRequest("mount requires an array".into()))?
        .iter()
        .map(|value| {
            let object = value
                .as_object()
                .filter(|object| object.len() == 2)
                .ok_or_else(|| Error::BadRequest("invalid proxy fields".into()))?;
            let location = object
                .get("location")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::BadRequest("proxy location must be a string".into()))?;
            let upstream = object
                .get("proxy_pass")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::BadRequest("proxy_pass must be a string".into()))?;
            Ok((location.to_owned(), upstream.to_owned()))
        })
        .collect::<Result<Vec<_>>>()?;
    parse_proxies(values).map_err(|error| Error::BadRequest(error.to_string()))
}

fn upstream_text(proxy: &ProxyLocation) -> String {
    format!(
        "{}://{}{}",
        proxy.proxy_pass.scheme.as_ref().unwrap(),
        proxy.proxy_pass.authority.as_ref().unwrap(),
        proxy
            .proxy_pass
            .path_and_query
            .as_ref()
            .map_or("", |path| path.as_str()),
    )
}
