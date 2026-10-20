//! Inspect, preserve and initialize the identity authorization database.
use std::sync::Arc;

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
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='module')",
            [],
            |row| row.get(0),
        )?;
        if !module {
            let legacy: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name IN ('location_rule_sets','location_rules'))", [], |row| row.get(0))?;
            return Err(Error::InvalidConfig(if legacy {
                "unsupported or incomplete legacy ACL schema; original database preserved".into()
            } else {
                "unrecognized access database format; original database preserved".into()
            }));
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
        if !matches!(version, 0 | 1) {
            return Err(Error::InvalidConfig(format!(
                "unsupported access database version {version}; expected 0 or 1"
            )));
        }
        // Validate the library's required tables before allowing its migrations to write.
        for sql in [
            "SELECT id,name,subject_id,alias,class,grants,offers,requests,description,status,expired_after,updated_at,created_at FROM contacts LIMIT 0",
            "SELECT id,method,api,effect,grantee_type,grantee,updated_at,created_at FROM access_rules LIMIT 0",
            "SELECT id,request_id,visitor,visitor_sid,method,api,stage,reason,expired_after,updated_at,created_at FROM access_reviews LIMIT 0",
        ] {
            conn.prepare(sql)?;
        }
        if version == 1 {
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

fn legacy_access(path: &std::path::Path) -> Result<bool> {
    if !path.try_exists()? {
        return Ok(false);
    }
    if !path.symlink_metadata()?.file_type().is_file() {
        return Ok(false);
    }
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let tables = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if !tables.iter().any(|name| name == "location_rule_sets")
        || !tables.iter().any(|name| name == "location_rules")
        || tables.iter().any(|name| {
            !matches!(
                name.as_str(),
                "location_rule_sets" | "location_rules" | "migration"
            )
        })
    {
        return Ok(false);
    }
    // Recognize the published configuration schema, not merely similarly named tables.
    for sql in [
        "SELECT id,pattern,created_at,updated_at FROM location_rule_sets LIMIT 0",
        "SELECT id,location_id,action,exprs,created_at,updated_at FROM location_rules LIMIT 0",
    ] {
        if conn.prepare(sql).is_err() {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(super) async fn load_access(
    profile: &IdentityProfile,
    subject: &access_control::SubjectId,
) -> Result<Arc<access_control::AccessService>> {
    let path = profile.access_db_path();
    let legacy = legacy_access(&path).map_err(|error| {
        Error::InvalidConfig(format!("access database {}: {error}", path.display()))
    })?;
    let version = if legacy { None } else { access_version(&path)? };
    let load = || async {
        let uri = format!("sqlite://{}?mode=rw", path.display());
        let access = access_control::AccessService::load_from_db(&uri, profile.name(), subject)
            .await
            .map(Arc::new)
            .map_err(|error| {
                Error::InvalidConfig(format!("access database {}: {error}", path.display()))
            })?;
        if rustix::process::geteuid().is_root() {
            let directory = std::fs::File::open(profile.db_dir())?;
            repair_identity_ownership(&directory, &directory.metadata()?)?;
        }
        Ok(access)
    };
    if version == Some(1) {
        return load().await;
    }
    // Nothing partially initialized is ever published at the application's database path.
    let staging = tempfile::Builder::new()
        .prefix(".access-init-")
        .tempdir_in(profile.db_dir())?;
    inherit_identity_owner(&std::fs::File::open(staging.path())?, profile)?;
    let working = tempfile::NamedTempFile::new_in(staging.path())?;
    inherit_identity_owner(working.as_file(), profile)?;
    let uri = format!("sqlite://{}?mode=rw", working.path().display());
    let access = access_control::AccessService::load_from_db(&uri, profile.name(), subject).await?;
    access
        .set_policy(
            access_control::Method::Specified(http::Method::POST),
            "/std/contact",
            access_control::Effect::Allow,
            access_control::Grantee::Named,
        )
        .await?;
    // Discovery is public before a contact relationship exists. The profile
    // GET prefix covers the public profile, avatar and introduction context.
    for (method, path) in [
        (http::Method::GET, "/std/acl/promissions"),
        (http::Method::GET, "/std/profile"),
        (http::Method::HEAD, "/std/profile/context"),
        // The public /workspace/intro page and assets are served by Workspace.
        (http::Method::GET, "/workspace"),
        (http::Method::HEAD, "/workspace"),
    ] {
        access
            .set_policy(
                access_control::Method::Specified(method),
                path,
                access_control::Effect::Allow,
                access_control::Grantee::All,
            )
            .await?;
    }
    let ready = tempfile::NamedTempFile::new_in(staging.path())?;
    inherit_identity_owner(ready.as_file(), profile)?;
    snapshot_access(working.path(), ready.path())?;
    if access_version(ready.path())? != Some(1) {
        return Err(Error::InvalidConfig(
            "initialized access database failed version validation".into(),
        ));
    }
    // Old configuration is archived, never translated or imported into the new database.
    if legacy || version == Some(0) {
        let prefix = if legacy {
            "access-legacy-backup-"
        } else {
            "access-v0-backup-"
        };
        let backup = tempfile::Builder::new()
            .prefix(prefix)
            .suffix(".db")
            .tempfile_in(profile.db_dir())?;
        inherit_identity_owner(backup.as_file(), profile)?;
        snapshot_access(&path, backup.path())?;
        let (_, backup_path) = backup.keep().map_err(|error| error.error)?;
        tracing::info!(
            database = %path.display(),
            backup = %backup_path.display(),
            "access database backed up"
        );
        // SQLite performs the replacement as a database transaction, including
        // existing journals/WAL, rather than renaming only the main database file.
        if legacy_access(&path)? != legacy || (!legacy && access_version(&path)? != version) {
            return Err(Error::InvalidConfig(format!(
                "{} changed during initialization; backup retained",
                path.display()
            )));
        }
        snapshot_access(ready.path(), &path)?;
        #[cfg(unix)]
        std::fs::File::open(profile.db_dir())?.sync_all()?;
        return load().await;
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
    std::fs::File::open(profile.db_dir())?.sync_all()?;
    load().await
}
