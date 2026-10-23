//! Inspect, preserve and initialize the identity authorization database.
use std::sync::Arc;

use access_control::{Effect, Grantee, Method};
use dhttp_home::identity::IdentityProfile;

use super::files::{inherit_identity_owner, repair_identity_ownership};
use crate::{Error, Result};

// None is a missing or empty database, never a malformed or unrecognized one.
pub(super) fn access_version(path: &std::path::Path) -> Result<Option<i64>> {
    match path.symlink_metadata() {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(Error::InvalidConfig(format!(
                "{} must be a regular database file",
                path.display()
            )));
        }
        Ok(_) => {}
    }
    let inspect = || -> Result<Option<i64>> {
        let conn = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let integrity: String = conn.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            return Err(Error::InvalidConfig(integrity));
        }
        let objects: i64 = conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )?;
        let user_version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if objects == 0 && user_version == 0 {
            return Ok(None);
        }
        let module: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_list WHERE type='table' AND name='module')",
            [],
            |row| row.get(0),
        )?;
        if !module && conn.query_row("SELECT count(*)=2 FROM sqlite_master WHERE name IN ('lib_capability','lib_capability_operation')",[],|r|r.get::<_,bool>(0))? && objects==2 {
            return Ok(None);
        }
        if !module {
            return Err(Error::InvalidConfig(
                "unrecognized access database format; original database preserved".into(),
            ));
        }
        let rows: i64 = conn.query_row("SELECT count(*) FROM module", [], |row| row.get(0))?;
        if rows != 1 {
            return Err(Error::InvalidConfig(
                "access module must contain exactly one row".into(),
            ));
        }
        let version: i64 = conn.query_row(
            "SELECT version FROM module WHERE module_name='access'",
            [],
            |row| row.get(0),
        )?;
        if !matches!(version, 0 | 1 | 2) {
            return Err(Error::InvalidConfig(format!(
                "unsupported access database version {version}; expected 0, 1 or 2"
            )));
        }
        // Validate the source schema before allowing structural migrations.
        for sql in [
            "SELECT id,name,subject_id,alias,class,grants,offers,requests,description,status,expired_after,updated_at,created_at FROM contacts LIMIT 0",
            "SELECT id,method,api,effect,grantee_type,grantee,updated_at,created_at FROM access_rules LIMIT 0",
            "SELECT id,request_id,visitor,visitor_sid,method,api,stage,reason,expired_after,updated_at,created_at FROM access_reviews LIMIT 0",
        ] {
            conn.prepare(sql)?;
        }
        if version == 2 {
            conn.prepare("SELECT name,updated_at FROM contact_deletions LIMIT 0")?;
        }
        if version >= 1 {
            conn.prepare("SELECT application_id,applicant_name,applicant_sid,payload_hash,status,received_at,expired_after FROM contact_applications LIMIT 0")?;
        } else {
            let applications: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='contact_applications')",
                [],
                |row| row.get(0),
            )?;
            if applications {
                return Err(Error::InvalidConfig(
                    "access v0 already contains v1 tables".into(),
                ));
            }
        }
        Ok(Some(version))
    };
    inspect().map_err(|error| {
        Error::InvalidConfig(format!("access database {}: {error}", path.display()))
    })
}

fn snapshot_access(source: &std::path::Path, destination: &std::path::Path) -> Result<()> {
    let blank = match std::fs::metadata(destination) {
        Ok(metadata) => metadata.len() == 0,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(error) => return Err(error.into()),
    };
    let conn =
        rusqlite::Connection::open_with_flags(source, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.backup(rusqlite::DatabaseName::Main, destination, None)?;
    let snapshot = rusqlite::Connection::open(destination)?;
    if blank {
        snapshot.pragma_update(None, "journal_mode", "DELETE")?;
    }
    let integrity: String = snapshot.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(Error::InvalidConfig(format!(
            "database snapshot failed: {integrity}"
        )));
    }
    snapshot
        .close()
        .map_err(|(_, error)| Error::ConfigDatabase(error))?;
    std::fs::File::open(destination)?.sync_all()?;
    Ok(())
}

pub(super) async fn load_access(
    profile: &IdentityProfile,
    subject: &access_control::SubjectId,
) -> Result<Arc<access_control::AccessService>> {
    let path = profile.join(".volume/sys/access/database");
    let version = access_version(&path)?;
    let load = || async {
        let uri = format!("sqlite://{}?mode=rw", path.display());
        let access = access_control::AccessService::load_from_db(&uri, profile.name(), subject)
            .await
            .map(Arc::new)
            .map_err(|error| {
                Error::InvalidConfig(format!("access database {}: {error}", path.display()))
            })?;
        if rustix::process::geteuid().is_root() {
            let directory = std::fs::File::open(profile.join(".volume/sys/access"))?;
            repair_identity_ownership(&directory, &directory.metadata()?)?;
        }
        Ok(access)
    };
    if version.is_some() {
        return load().await;
    }
    // Nothing partially initialized is ever published at the application's database path.
    let staging = tempfile::Builder::new()
        .prefix(".access-init-")
        .tempdir_in(profile.join(".volume/sys/access"))?;
    inherit_identity_owner(&std::fs::File::open(staging.path())?, profile)?;
    let working = tempfile::NamedTempFile::new_in(staging.path())?;
    inherit_identity_owner(working.as_file(), profile)?;
    let uri = format!("sqlite://{}?mode=rw", working.path().display());
    let access = access_control::AccessService::load_from_db(&uri, profile.name(), subject).await?;
    // Seed native discovery/contact entry points only; business ACLs are configured
    // after installation. Existing databases never have defaults reapplied.
    for (method, path, effect, grantee) in [
        (
            http::Method::POST,
            "/std/contact",
            Effect::Allow,
            Grantee::Named,
        ),
        (
            http::Method::GET,
            "/std/contact",
            Effect::Allow,
            Grantee::Named,
        ),
        (
            http::Method::GET,
            "/std/acl/status",
            Effect::Allow,
            Grantee::Named,
        ),
        (
            http::Method::GET,
            "/std/contact/list",
            Effect::Deny,
            Grantee::All,
        ),
        (
            http::Method::GET,
            "/std/acl/permissions",
            Effect::Allow,
            Grantee::All,
        ),
    ] {
        access
            .set_policy(Method::Specified(method), path, effect, grantee)
            .await?;
    }
    if path.try_exists()? {
        let pending = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let capabilities: bool = pending.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='lib_capability')",
            [],
            |r| r.get(0),
        )?;
        if capabilities {
            drop(pending);
            let mut db = rusqlite::Connection::open(working.path())?;
            db.execute(
                "ATTACH DATABASE ?1 AS pending",
                [path
                    .to_str()
                    .ok_or_else(|| Error::InvalidConfig("invalid ACL path".into()))?],
            )?;
            let tx = db.transaction()?;
            let schema = include_str!("../package/sql/schema-v3.sql")
                .split("CREATE TABLE IF NOT EXISTS acl.lib_capability (")
                .nth(1)
                .unwrap();
            tx.execute_batch(&format!(
                "CREATE TABLE IF NOT EXISTS lib_capability ({}",
                schema.replace("acl.lib_capability_operation", "lib_capability_operation")
            ))?;
            tx.execute_batch("INSERT INTO main.lib_capability SELECT * FROM pending.lib_capability; INSERT INTO main.lib_capability_operation SELECT * FROM pending.lib_capability_operation;")?;
            tx.commit()?;
        }
    }
    let ready = tempfile::NamedTempFile::new_in(staging.path())?;
    inherit_identity_owner(ready.as_file(), profile)?;
    snapshot_access(working.path(), ready.path())?;
    if access_version(ready.path())? != Some(2) {
        return Err(Error::InvalidConfig(
            "initialized access database failed version validation".into(),
        ));
    }
    // A simultaneous initializer must not overwrite a completed database or user rules.
    if access_version(&path)? != version {
        return load().await;
    }
    if path.try_exists()? {
        // Restore through SQLite when recovering an empty file, so any existing
        // journal/WAL is handled by SQLite rather than replacing only the main file.
        snapshot_access(ready.path(), &path)?;
    } else {
        match std::fs::hard_link(ready.path(), &path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return load().await,
            Err(error) => return Err(error.into()),
        }
    }
    #[cfg(unix)]
    std::fs::File::open(profile.join(".volume/sys/access"))?.sync_all()?;
    load().await
}
