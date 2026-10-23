//! Contact database initialization and outbound request records.
use std::time::Duration;

use dhttp_home::identity::IdentityProfile;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde_json::{Value, json};

use super::{Failure, fail, now};

pub(super) fn storage(error: rusqlite::Error) -> Failure {
    let status = if matches!(&error, rusqlite::Error::SqliteFailure(code, _) if matches!(code.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
    {
        503
    } else {
        500
    };
    fail(status, format!("contact storage: {error}"))
}

pub(super) fn outbound_request(db: &Connection, id: i64) -> Result<Value, Failure> {
    db.query_row("SELECT id,target_name,description,requested_capabilities,offered_capabilities,status,expired_after,delivery_deadline,remote_expired_after,last_checked_at,error_message,created_at,updated_at FROM outbound_contact_request WHERE id=?1",[id],|r|Ok(json!({"id":r.get::<_,i64>(0)?,"target_name":r.get::<_,String>(1)?,"description":r.get::<_,String>(2)?,"requested_capabilities":serde_json::from_str::<Value>(&r.get::<_,String>(3)?).unwrap_or(Value::Null),"offered_capabilities":serde_json::from_str::<Value>(&r.get::<_,String>(4)?).unwrap_or(Value::Null),"status":r.get::<_,String>(5)?,"valid_before":r.get::<_,i64>(6)?,"delivery_deadline":r.get::<_,i64>(7)?,"remote_valid_before":r.get::<_,Option<i64>>(8)?,"last_checked_at":r.get::<_,Option<i64>>(9)?,"error_message":r.get::<_,Option<String>>(10)?,"created_at":r.get::<_,i64>(11)?,"updated_at":r.get::<_,i64>(12)?}))).optional().map_err(storage)?.ok_or_else(||fail(404,"contact request not found"))
}
pub(super) fn expire_requests(db: &Connection) -> Result<(), Failure> {
    db.execute("UPDATE outbound_contact_request SET status=CASE WHEN status='queued' THEN 'failed' ELSE 'expired' END,lease_until=NULL,updated_at=?1,error_message='contact request expired' WHERE status IN ('queued','pending') AND expired_after<=?1",[now()]).map_err(storage)?;
    Ok(())
}

pub(crate) fn database(profile: &IdentityProfile) -> Result<Connection, Failure> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let directory = profile.join(".volume/sys/contact");
    std::fs::create_dir_all(&directory).map_err(|e| fail(500, e.to_string()))?;
    let path = directory.join("database");
    match path.symlink_metadata() {
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(fail(500, "contact database must be a regular file"));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(file) => {
                    if rustix::process::geteuid().is_root() {
                        use std::os::unix::fs::MetadataExt;
                        let owner = profile
                            .join(".volume/sys/contact")
                            .metadata()
                            .map_err(|error| fail(500, error.to_string()))?;
                        rustix::fs::fchown(
                            &file,
                            Some(rustix::fs::Uid::from_raw(owner.uid())),
                            Some(rustix::fs::Gid::from_raw(owner.gid())),
                        )
                        .map_err(|error| fail(500, error.to_string()))?;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(fail(500, e.to_string())),
            }
        }
        Err(e) => return Err(fail(500, e.to_string())),
        _ => {}
    }
    if !path
        .symlink_metadata()
        .map_err(|e| fail(500, e.to_string()))?
        .file_type()
        .is_file()
    {
        return Err(fail(500, "contact database must be a regular file"));
    }
    let file = std::fs::File::from(
        rustix::fs::open(
            &path,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(|error| fail(500, error.to_string()))?,
    );
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file
            .metadata()
            .map_err(|error| fail(500, error.to_string()))?;
        if !metadata.is_file() || metadata.nlink() != 1 {
            return Err(fail(
                500,
                "contact database must be a single-link regular file",
            ));
        }
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|e| fail(500, e.to_string()))?;
    let mut db = Connection::open(&path).map_err(storage)?;
    db.busy_timeout(Duration::from_secs(2)).map_err(storage)?;
    db.pragma_update(None, "synchronous", "FULL")
        .map_err(storage)?;
    let tx = db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(storage)?;
    let version: i64 = tx
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(storage)?;
    if version == 0 {
        let count: i64 = tx
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
                [],
                |r| r.get(0),
            )
            .map_err(storage)?;
        if count != 0 {
            return Err(fail(500, "unrecognized contact database"));
        }
        tx.execute_batch(include_str!("sql/schema-v4.sql"))
            .map_err(storage)?;
        tx.pragma_update(None, "user_version", 4).map_err(storage)?;
    } else if version == 1 {
        tx.execute_batch(include_str!("sql/migrate-v1-to-v2.sql"))
            .map_err(storage)?;
        tx.pragma_update(None, "user_version", 2).map_err(storage)?;
    } else if version != 2 && version != 3 && version != 4 {
        return Err(fail(500, "unsupported contact database version"));
    }
    if version == 1 || version == 2 {
        tx.execute_batch(include_str!("sql/migrate-v2-to-v3.sql"))
            .map_err(storage)?;
        tx.pragma_update(None, "user_version", 3).map_err(storage)?;
    }
    if (1..=3).contains(&version) {
        tx.execute_batch(include_str!("sql/migrate-v3-to-v4.sql"))
            .map_err(storage)?;
        tx.pragma_update(None, "user_version", 4).map_err(storage)?;
    }
    tx.prepare("SELECT id,target_name,description,requested_capabilities,offered_capabilities,application_id,status,expired_after,delivery_deadline,remote_expired_after,next_attempt_at,attempt_count,lease_until,last_checked_at,error_message,created_at,updated_at,contact_id,contact_subject,requested_access,offers FROM outbound_contact_request LIMIT 0").map_err(storage)?;
    tx.commit().map_err(storage)?;
    if rustix::process::geteuid().is_root() {
        use std::os::unix::fs::MetadataExt;

        use rustix::fs::{Mode, OFlags};
        let directory = std::fs::File::from(
            rustix::fs::open(
                profile.join(".volume/sys/contact"),
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|error| fail(500, error.to_string()))?,
        );
        let owner = directory
            .metadata()
            .map_err(|error| fail(500, error.to_string()))?;
        for name in ["database", "database-wal", "database-shm"] {
            let file = match rustix::fs::openat(
                &directory,
                name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(file) => std::fs::File::from(file),
                Err(rustix::io::Errno::NOENT) => continue,
                Err(error) => return Err(fail(500, error.to_string())),
            };
            let metadata = file
                .metadata()
                .map_err(|error| fail(500, error.to_string()))?;
            if !metadata.is_file() || metadata.nlink() != 1 {
                return Err(fail(
                    500,
                    "contact database files must be single-link regular files",
                ));
            }
            rustix::fs::fchown(
                &file,
                Some(rustix::fs::Uid::from_raw(owner.uid())),
                Some(rustix::fs::Gid::from_raw(owner.gid())),
            )
            .map_err(|error| fail(500, error.to_string()))?;
        }
    }
    Ok(db)
}
