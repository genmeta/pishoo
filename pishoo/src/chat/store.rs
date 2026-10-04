use std::path::{Path, PathBuf};

use dhttp_home::identity::IdentityProfile;
use sea_orm::{
    ConnectOptions, ConnectionTrait, Database, DatabaseBackend, DatabaseConnection, Statement,
    TransactionTrait,
};
use snafu::{ResultExt, Snafu};

const SCHEMA_VERSION: i64 = 4;

#[derive(Debug, Snafu)]
pub enum StoreError {
    #[snafu(display("failed to create Chat database directory `{}`", path.display()))]
    Directory {
        path: PathBuf,
        source: std::io::Error,
    },
    #[snafu(display("failed to open Chat database `{}`", path.display()))]
    Connect {
        path: PathBuf,
        source: sea_orm::DbErr,
    },
    #[snafu(display("failed to migrate Chat database `{}`", path.display()))]
    Migration {
        path: PathBuf,
        source: sea_orm::DbErr,
    },
    #[snafu(display("unsupported Chat database version {version} in `{}`", path.display()))]
    UnsupportedVersion { path: PathBuf, version: i64 },
}

pub struct ChatStore {
    db: DatabaseConnection,
}

impl ChatStore {
    pub async fn open(profile: &IdentityProfile) -> Result<Self, StoreError> {
        let path = profile.db_dir().join("chat.db");
        let parent = path.parent().expect("Chat database path has a parent");
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
        Ok(Self { db })
    }

    pub(crate) fn db(&self) -> &DatabaseConnection {
        &self.db
    }
}

async fn migrate(db: &DatabaseConnection, path: &Path) -> Result<(), StoreError> {
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
        transaction
            .execute_unprepared(include_str!("migrations/1.sql"))
            .await
            .context(MigrationSnafu {
                path: path.to_path_buf(),
            })?;
        transaction
            .execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "INSERT INTO module_versions(module_name,version) VALUES ('chat', ?)",
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
                "SELECT version FROM module_versions WHERE module_name='chat'".to_owned(),
            ))
            .await
            .context(MigrationSnafu {
                path: path.to_path_buf(),
            })?
            .ok_or_else(|| StoreError::Migration {
                path: path.to_path_buf(),
                source: sea_orm::DbErr::Custom(
                    "missing chat schema version; database preserved".into(),
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
        "SELECT contact_name,updated_at FROM chat_conversations LIMIT 0",
        "SELECT contact_name,subject_id,remote_message_granted,updated_at FROM chat_capability_state LIMIT 0",
        "SELECT id,contact_name,client_message_id,remote_message_id,direction,state,sender_name,recipient_name,recipient_subject_id,text,attempt_count,last_attempt_at,next_attempt_at,delivered_at,created_at,updated_at,error_message FROM chat_messages LIMIT 0",
        "SELECT id,kind,contact_name,message_id,state,available_at,attempt_count,lease_token,lease_until,last_error,created_at,updated_at FROM chat_jobs LIMIT 0",
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
    transaction.commit().await.context(MigrationSnafu {
        path: path.to_path_buf(),
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use dhttp_home::identity::IdentityProfile;
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

    use super::ChatStore;

    #[tokio::test]
    async fn opens_fresh_profile_local_database() {
        let root = std::env::temp_dir().join(format!(
            "pishoo-chat-store-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time")
                .as_nanos()
        ));
        let profile = IdentityProfile::try_from(root.join("owner.example")).expect("profile");
        let store = ChatStore::open(&profile).await.expect("chat store");
        let version = store
            .db()
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT version FROM module_versions WHERE module_name = 'chat'".to_owned(),
            ))
            .await
            .expect("read schema version")
            .expect("chat schema version");
        assert_eq!(version.try_get::<i64>("", "version").expect("version"), 4);
        let capability_state = store
            .db()
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'chat_capability_state'".to_owned(),
            ))
            .await
            .expect("read capability state table")
            .expect("capability state table");
        assert_eq!(
            capability_state
                .try_get::<String>("", "name")
                .expect("table name"),
            "chat_capability_state"
        );
        assert!(profile.join("db/chat.db").exists());
        drop(store);
        std::fs::remove_dir_all(root).expect("remove test profile");
    }
}
