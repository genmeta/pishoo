use std::path::{Path, PathBuf};

use dhttp::home::identity::IdentityProfile;
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
        let path = profile.join(IdentityProfile::DB_DIR).join("chat.db");
        let parent = path.parent().expect("Chat database path has a parent");
        std::fs::create_dir_all(parent).context(DirectorySnafu {
            path: parent.to_path_buf(),
        })?;
        let mut options = ConnectOptions::new(format!("sqlite://{}?mode=rwc", path.display()));
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
    transaction
        .execute_unprepared(include_str!("migrations/1.sql"))
        .await
        .context(MigrationSnafu {
            path: path.to_path_buf(),
        })?;
    let row = transaction
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT version FROM module_versions WHERE module_name = 'chat'".to_owned(),
        ))
        .await
        .context(MigrationSnafu {
            path: path.to_path_buf(),
        })?;
    match row {
        None => {
            transaction
                .execute_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    "INSERT INTO module_versions (module_name, version) VALUES ('chat', ?)",
                    [SCHEMA_VERSION.into()],
                ))
                .await
                .context(MigrationSnafu {
                    path: path.to_path_buf(),
                })?;
        }
        Some(row) => {
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
    }
    transaction.commit().await.context(MigrationSnafu {
        path: path.to_path_buf(),
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use dhttp::home::identity::IdentityProfile;
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
