//! Device-wide authority. Certificate owner hashes bind names to their owners.
use std::{path::Path, sync::Arc};

use access_control::Visitor;
use dhttp_home::DhttpHome;
use rusqlite::{Connection, OptionalExtension, params};
use tokio::sync::Mutex;

use crate::{Error, Result};

pub(crate) fn open(home: &DhttpHome) -> Result<Arc<Mutex<Connection>>> {
    private_directory(&home.join(".system"))?;
    let path = home.join(".system/database");
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    }
    let file = options.open(&path)?;
    if !file.metadata()?.is_file() {
        return Err(Error::InvalidConfig(
            "system database must be a regular file".into(),
        ));
    }
    let mut db = Connection::open(path)?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version == 0 {
        let objects: i64 = tx.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))?;
        if objects != 0 {
            return Err(Error::InvalidConfig("unrecognized system database".into()));
        }
        tx.execute_batch(include_str!("schema.sql"))?;
    } else if version == 1 {
        tx.execute_batch(include_str!("migrations/v2_unify_administrators.sql"))?;
    } else if version != 2 {
        return Err(Error::InvalidConfig(format!(
            "unsupported system database version {version}"
        )));
    }
    tx.prepare("SELECT name,subject_id FROM administrator LIMIT 0")?;
    tx.prepare("SELECT name,subject_id,phase FROM managed_user LIMIT 0")?;
    tx.prepare("SELECT id,first_administrator FROM device LIMIT 0")?;
    let integrity: String = tx.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    if integrity != "ok" {
        return Err(Error::InvalidConfig(integrity));
    }
    tx.commit()?;
    Ok(Arc::new(Mutex::new(db)))
}

pub(super) fn private_directory(path: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    if !path.symlink_metadata()?.file_type().is_dir() {
        return Err(Error::InvalidConfig(
            "system directory must be a real directory".into(),
        ));
    }
    Ok(())
}

pub(super) fn conflict(code: &'static str) -> Error {
    Error::PackagePolicy {
        code,
        status: http::StatusCode::CONFLICT,
    }
}

pub(super) fn canonical(name: &str) -> Result<String> {
    dhttp_home::normalize_name(name)
        .ok_or_else(|| Error::BadRequest("invalid identity name".into()))
}

/// Callers commit the insertion and binding check in their write transaction.
pub(super) fn put_admin(db: &Connection, name: &str, subject: &[u8]) -> Result<()> {
    db.execute(
        "INSERT INTO administrator(name,subject_id) VALUES(?1,?2) ON CONFLICT(name) DO NOTHING",
        params![name, subject],
    )?;
    let bound: Vec<u8> = db.query_row(
        "SELECT subject_id FROM administrator WHERE name=?1",
        [name],
        |row| row.get(0),
    )?;
    if bound != subject {
        return Err(conflict("administrator_owner_changed"));
    }
    Ok(())
}

/// The durable first registration prevents a revoked administrator from being re-enrolled.
pub(super) fn register_first_administrator(db: &Connection, owner: &Visitor) -> Result<()> {
    let registered: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM device WHERE id=1)",
        [],
        |row| row.get(0),
    )?;
    if registered {
        return Ok(());
    }
    let existing: Option<String> = db
        .query_row(
            "SELECT name FROM administrator ORDER BY name LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let first = if let Some(name) = existing {
        name
    } else {
        put_admin(db, owner.name(), owner.subject_id().as_bytes())?;
        owner.name().to_owned()
    };
    db.execute(
        "INSERT INTO device(id,first_administrator) VALUES(1,?1)",
        [first],
    )?;
    Ok(())
}

pub(super) fn administrator(db: &Connection, visitor: &Visitor) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM administrator WHERE name=?1 AND subject_id=?2)",
        params![visitor.name(), visitor.subject_id().as_bytes()],
        |r| r.get(0),
    )?)
}

/// The target is the verified local endpoint serving the request.
pub(super) fn authorize(db: &Connection, target: &Visitor, visitor: &Visitor) -> Result<()> {
    if !administrator(db, target)? || !administrator(db, visitor)? {
        return Err(Error::Denied);
    }
    Ok(())
}
