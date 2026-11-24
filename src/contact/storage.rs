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

const REQUEST_COLUMNS: &str = "target_name, application_id, description, state,
    min(local_deadline,coalesce(remote_deadline,local_deadline)), last_error, created_at,
    updated_at, contact_id, target_subject";

fn draft_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    Ok(json!({
        "target_name":row.get::<_,String>(0)?, "application_id":row.get::<_,String>(1)?,
        "description":row.get::<_,String>(2)?, "status":row.get::<_,String>(3)?,
        "valid_before":row.get::<_,i64>(4)?, "error_message":row.get::<_,Option<String>>(5)?,
        "created_at":row.get::<_,i64>(6)?, "updated_at":row.get::<_,i64>(7)?,
        "_contact_id":row.get::<_,Option<i64>>(8)?, "_contact_subject":row.get::<_,Option<String>>(9)?
    }))
}

pub(super) fn outbound_request(db: &Connection, target: &str) -> Result<Option<Value>, Failure> {
    db.query_row(
        &format!("SELECT {REQUEST_COLUMNS} FROM outbound_contact_request WHERE target_name=?1"),
        [target],
        draft_row,
    )
    .optional()
    .map_err(storage)
}

pub(super) fn expire_requests(db: &Connection) -> Result<(), Failure> {
    db.execute("UPDATE outbound_contact_request SET state='expired',lease_until=NULL,review_id=NULL,updated_at=?1,last_error='contact request expired'
        WHERE state IN ('queued','pending') AND min(local_deadline,coalesce(remote_deadline,local_deadline))<=?1",[now()]).map_err(storage)?;
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
        tx.execute_batch(include_str!("sql/schema.sql"))
            .map_err(storage)?;
        tx.pragma_update(None, "user_version", 7).map_err(storage)?;
    } else if version == 1 {
        tx.execute_batch(include_str!(
            "sql/migrations/v2_bind_contact_generation.sql"
        ))
        .map_err(storage)?;
        tx.pragma_update(None, "user_version", 2).map_err(storage)?;
    } else if !(2..=8).contains(&version) {
        return Err(fail(500, "unsupported contact database version"));
    }
    if version == 1 || version == 2 {
        tx.execute_batch(include_str!(
            "sql/migrations/v3_rename_outbound_request.sql"
        ))
        .map_err(storage)?;
        tx.pragma_update(None, "user_version", 3).map_err(storage)?;
    }
    if (1..=3).contains(&version) {
        tx.execute_batch(include_str!(
            "sql/migrations/v4_add_permission_snapshots.sql"
        ))
        .map_err(storage)?;
        tx.pragma_update(None, "user_version", 4).map_err(storage)?;
    }
    if (1..=4).contains(&version) {
        tx.execute_batch(include_str!("sql/migrations/v5_add_review_tracking.sql"))
            .map_err(storage)?;
        tx.pragma_update(None, "user_version", 5).map_err(storage)?;
    }
    if (1..=5).contains(&version) {
        tx.execute_batch(include_str!("sql/migrations/v6_add_directory_entries.sql"))
            .map_err(storage)?;
        tx.pragma_update(None, "user_version", 6).map_err(storage)?;
    }
    if (1..=6).contains(&version) {
        if tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM contact_entries WHERE kind='group')",
                [],
                |r| r.get::<_, bool>(0),
            )
            .map_err(storage)?
        {
            return Err(fail(
                409,
                "existing group memberships require offline Contact migration",
            ));
        }
        tx.execute_batch(include_str!("sql/migrations/v7_simplify_contacts.sql"))
            .map_err(storage)?;
        tx.pragma_update(None, "user_version", 7).map_err(storage)?;
    }
    if version < 7 {
        // Existing entries stay available to the cross-database migration until
        // it has verified their aliases, subject bindings and Profile avatars.
        let has_entries: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='contact_entries')",
                [],
                |r| r.get(0),
            )
            .map_err(storage)?;
        if !has_entries
            || !tx
                .query_row("SELECT EXISTS(SELECT 1 FROM contact_entries)", [], |r| {
                    r.get::<_, bool>(0)
                })
                .map_err(storage)?
        {
            tx.execute_batch("DROP TABLE IF EXISTS contact_entries; DROP TABLE IF EXISTS directory_service_grants; PRAGMA user_version=8;").map_err(storage)?;
        }
    }
    tx.prepare("SELECT target_name,application_id,contact_id,target_subject,description,requested,offered,state,local_deadline,remote_deadline,review_id,next_attempt_at,lease_until,last_error,created_at,updated_at FROM outbound_contact_request LIMIT 0").map_err(storage)?;
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
