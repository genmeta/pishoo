use std::path::PathBuf;

use dhttp_home::identity::IdentityProfile;
use sea_orm::{
    ConnectOptions, ConnectionTrait, Database, DatabaseBackend, DatabaseConnection, Statement,
    TransactionTrait,
};
use snafu::{ResultExt, Snafu};

const SCHEMA_VERSION: i64 = 8;

#[derive(Debug, Snafu)]
pub enum StoreError {
    #[snafu(display("failed to create Workspace database directory `{}`", path.display()))]
    Directory {
        path: PathBuf,
        source: std::io::Error,
    },
    #[snafu(display("failed to open Workspace database `{}`", path.display()))]
    Connect {
        path: PathBuf,
        source: sea_orm::DbErr,
    },
    #[snafu(display("failed to migrate Workspace database `{}`", path.display()))]
    Migration {
        path: PathBuf,
        source: sea_orm::DbErr,
    },
    #[snafu(display("unsupported Workspace database version {version} in `{}`", path.display()))]
    UnsupportedVersion { path: PathBuf, version: i64 },
}

pub struct WorkspaceStore {
    db: DatabaseConnection,
    profile_assets: PathBuf,
}

impl WorkspaceStore {
    pub async fn open(profile: &IdentityProfile) -> Result<Self, StoreError> {
        let path = profile.db_dir().join("workspace.db");
        let parent = path.parent().expect("Workspace database path has a parent");
        let prepare = || -> std::io::Result<()> {
            let mut directory = std::fs::DirBuilder::new();
            directory.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                directory.mode(0o700);
            }
            directory.create(parent)?;
            if !parent.symlink_metadata()?.file_type().is_dir() {
                return Err(std::io::Error::other(
                    "database directory must not be a symlink",
                ));
            }
            let mut file = std::fs::OpenOptions::new();
            file.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                file.mode(0o600);
            }
            match file.open(&path) {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
            if !path.symlink_metadata()?.file_type().is_file() {
                return Err(std::io::Error::other(
                    "database must be a regular file, not a symlink",
                ));
            }
            Ok(())
        };
        prepare().context(DirectorySnafu { path: path.clone() })?;
        let mut options = ConnectOptions::new(format!("sqlite://{}?mode=rw", path.display()));
        options.max_connections(1);
        options.sqlx_logging(false);
        let db = Database::connect(options)
            .await
            .context(ConnectSnafu { path: path.clone() })?;
        for pragma in ["PRAGMA foreign_keys = ON", "PRAGMA busy_timeout = 5000"] {
            db.execute_unprepared(pragma)
                .await
                .context(ConnectSnafu { path: path.clone() })?;
        }
        migrate(&db, &path).await?;
        let profile_assets = profile.join("assets/profile");
        for directory in [profile.join("assets"), profile_assets.clone()] {
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&directory) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(source) => {
                    return Err(StoreError::Directory {
                        path: directory,
                        source,
                    });
                }
            }
            if !directory
                .symlink_metadata()
                .context(DirectorySnafu {
                    path: directory.clone(),
                })?
                .file_type()
                .is_dir()
            {
                return Err(StoreError::Directory {
                    path: directory,
                    source: std::io::Error::other("profile assets directory must not be a symlink"),
                });
            }
        }
        Ok(Self { db, profile_assets })
    }

    pub(crate) fn db(&self) -> &DatabaseConnection {
        &self.db
    }

    pub(crate) fn profile_assets(&self) -> &std::path::Path {
        &self.profile_assets
    }
}

async fn migrate(db: &DatabaseConnection, path: &PathBuf) -> Result<(), StoreError> {
    let transaction = db.begin().await.context(MigrationSnafu {
        path: path.to_path_buf(),
    })?;
    for row in transaction
        .query_all_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "PRAGMA quick_check".to_owned(),
        ))
        .await
        .context(MigrationSnafu {
            path: path.to_path_buf(),
        })?
    {
        let integrity: String = row.try_get("", "quick_check").context(MigrationSnafu {
            path: path.to_path_buf(),
        })?;
        if integrity != "ok" {
            return Err(StoreError::Migration {
                path: path.to_path_buf(),
                source: sea_orm::DbErr::Custom(integrity),
            });
        }
    }
    let row = transaction
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT count(*) AS objects FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'"
                .to_owned(),
        ))
        .await
        .context(MigrationSnafu {
            path: path.to_path_buf(),
        })?
        .expect("aggregate row");
    let objects: i64 = row.try_get("", "objects").context(MigrationSnafu {
        path: path.to_path_buf(),
    })?;
    if objects == 0 {
        let row = transaction
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "PRAGMA user_version".to_owned(),
            ))
            .await
            .context(MigrationSnafu {
                path: path.to_path_buf(),
            })?
            .expect("pragma row");
        let version: i64 = row.try_get("", "user_version").context(MigrationSnafu {
            path: path.to_path_buf(),
        })?;
        if version != 0 {
            return Err(StoreError::UnsupportedVersion {
                path: path.to_path_buf(),
                version,
            });
        }
        transaction.execute_unprepared(
            "CREATE TABLE module_versions (module_name TEXT PRIMARY KEY CHECK(length(module_name)>0), version INTEGER NOT NULL CHECK(version>=1))"
        ).await.context(MigrationSnafu { path: path.to_path_buf() })?;
        transaction
            .execute_unprepared(include_str!("migrations/0.sql"))
            .await
            .context(MigrationSnafu {
                path: path.to_path_buf(),
            })?;
        transaction
            .execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "INSERT INTO module_versions(module_name,version) VALUES ('workspace', ?)",
                [SCHEMA_VERSION.into()],
            ))
            .await
            .context(MigrationSnafu {
                path: path.to_path_buf(),
            })?;
    } else {
        // Read the version before executing DDL: unknown databases are never repaired or adopted.
        let row = transaction
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT version FROM module_versions WHERE module_name='workspace'".to_owned(),
            ))
            .await
            .context(MigrationSnafu {
                path: path.to_path_buf(),
            })?
            .ok_or_else(|| StoreError::Migration {
                path: path.to_path_buf(),
                source: sea_orm::DbErr::Custom(
                    "missing workspace schema version; database preserved".into(),
                ),
            })?;
        let version: i64 = row.try_get("", "version").context(MigrationSnafu {
            path: path.to_path_buf(),
        })?;
        if version != SCHEMA_VERSION {
            return Err(StoreError::UnsupportedVersion {
                path: path.to_path_buf(),
                version,
            });
        }
    }
    for sql in [
        "SELECT id,display_name,avatar_name,gender,updated_at FROM profile_preferences LIMIT 0",
        "SELECT id,target_name,description,requested_capabilities,offered_capabilities,application_id,sender_subject_id,recipient_subject_id,status,expired_after,delivery_deadline,remote_expired_after,next_attempt_at,attempt_count,lease_until,last_checked_at,error_message,created_at,updated_at FROM outbound_contact_requests LIMIT 0",
        "SELECT contact_name,capability_id,decision,descriptor_version,subject_id,request_id,updated_at FROM capability_decisions LIMIT 0",
        "SELECT id,contact_name,capability_id,decision,descriptor_version,subject_id,request_id,decided_at FROM capability_decision_events LIMIT 0",
        "SELECT contact_name,contact_id,subject_id,saved_at FROM saved_contacts LIMIT 0",
    ] {
        transaction
            .query_all_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                sql.to_owned(),
            ))
            .await
            .context(MigrationSnafu {
                path: path.to_path_buf(),
            })?;
    }
    let row = transaction
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT count(*) AS total FROM profile_preferences WHERE id=1".to_owned(),
        ))
        .await
        .context(MigrationSnafu {
            path: path.to_path_buf(),
        })?
        .expect("aggregate row");
    if row.try_get::<i64>("", "total").context(MigrationSnafu {
        path: path.to_path_buf(),
    })? != 1
    {
        return Err(StoreError::Migration {
            path: path.to_path_buf(),
            source: sea_orm::DbErr::Custom(
                "missing profile preferences; database preserved".into(),
            ),
        });
    }
    transaction.commit().await.context(MigrationSnafu {
        path: path.to_path_buf(),
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use dhttp_home::identity::IdentityProfile;
    use sea_orm::{ConnectionTrait, Database, Statement};

    use super::{StoreError, WorkspaceStore};

    #[tokio::test]
    async fn fresh_schema_is_idempotent_and_profiles_are_isolated() {
        let root = std::env::temp_dir().join(format!(
            "pishoo-workspace-store-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time is valid")
                .as_nanos()
        ));
        let alice = IdentityProfile::try_from(root.join("alice.example")).expect("valid profile");
        let bob = IdentityProfile::try_from(root.join("bob.example")).expect("valid profile");

        let first = WorkspaceStore::open(&alice).await.expect("alice store");
        first
            .db()
            .execute_raw(Statement::from_string(
                sea_orm::DatabaseBackend::Sqlite,
                "UPDATE profile_preferences SET display_name = 'Alice' WHERE id = 1".to_owned(),
            ))
            .await
            .expect("update alice");
        let reopened = WorkspaceStore::open(&alice)
            .await
            .expect("reopen alice store");
        let other = WorkspaceStore::open(&bob).await.expect("bob store");

        for store in [&reopened, &other] {
            let row = store
                .db()
                .query_one_raw(Statement::from_string(
                    sea_orm::DatabaseBackend::Sqlite,
                    "SELECT version FROM module_versions WHERE module_name = 'workspace'"
                        .to_owned(),
                ))
                .await
                .expect("read schema version")
                .expect("workspace schema version");
            assert_eq!(row.try_get::<i64>("", "version").expect("version"), 8);
        }
        let row = other
            .db()
            .query_one_raw(Statement::from_string(
                sea_orm::DatabaseBackend::Sqlite,
                "SELECT display_name FROM profile_preferences WHERE id = 1".to_owned(),
            ))
            .await
            .expect("read bob profile")
            .expect("bob preferences");
        assert_eq!(
            row.try_get::<Option<String>>("", "display_name")
                .expect("name"),
            None
        );

        let unsupported_profile =
            IdentityProfile::try_from(root.join("unsupported.example")).expect("valid profile");
        std::fs::create_dir_all(unsupported_profile.join("db"))
            .expect("create unsupported database dir");
        let unsupported_db = Database::connect(format!(
            "sqlite://{}?mode=rwc",
            unsupported_profile.join("db/workspace.db").display()
        ))
        .await
        .expect("open unsupported database");
        unsupported_db
            .execute_unprepared(
                "CREATE TABLE module_versions (module_name TEXT PRIMARY KEY, version INTEGER NOT NULL); \
                 INSERT INTO module_versions VALUES ('workspace', 4)",
            )
            .await
            .expect("seed unsupported schema version");
        unsupported_db
            .close()
            .await
            .expect("close unsupported database");
        assert!(matches!(
            WorkspaceStore::open(&unsupported_profile).await,
            Err(StoreError::UnsupportedVersion { version: 4, .. })
        ));

        drop((first, reopened, other));
        std::fs::remove_dir_all(root).expect("remove isolated test profiles");
    }
}
