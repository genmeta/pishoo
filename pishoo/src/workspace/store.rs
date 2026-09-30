use std::path::PathBuf;

use dhttp::home::identity::IdentityProfile;
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
        let path = profile.join(IdentityProfile::DB_DIR).join("workspace.db");
        let parent = path.parent().expect("Workspace database path has a parent");
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
        let profile_assets = profile.join("assets/profile");
        std::fs::create_dir_all(&profile_assets).context(DirectorySnafu {
            path: profile_assets.clone(),
        })?;
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
    let transaction = db
        .begin()
        .await
        .context(MigrationSnafu { path: path.clone() })?;
    transaction
        .execute_unprepared(
            "CREATE TABLE IF NOT EXISTS module_versions (\
             module_name TEXT PRIMARY KEY CHECK (length(module_name) > 0), \
             version INTEGER NOT NULL CHECK (version >= 1))",
        )
        .await
        .context(MigrationSnafu { path: path.clone() })?;
    let row = transaction
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT version FROM module_versions WHERE module_name = 'workspace'".to_owned(),
        ))
        .await
        .context(MigrationSnafu { path: path.clone() })?;
    match row {
        None => {
            transaction
                .execute_unprepared(include_str!("migrations/0.sql"))
                .await
                .context(MigrationSnafu { path: path.clone() })?;
            transaction
                .execute_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    "INSERT INTO module_versions (module_name, version) VALUES ('workspace', ?)",
                    [SCHEMA_VERSION.into()],
                ))
                .await
                .context(MigrationSnafu { path: path.clone() })?;
        }
        Some(row) => {
            let version: i64 = row
                .try_get("", "version")
                .context(MigrationSnafu { path: path.clone() })?;
            if version != SCHEMA_VERSION {
                return Err(StoreError::UnsupportedVersion {
                    path: path.clone(),
                    version,
                });
            }
        }
    }
    transaction
        .commit()
        .await
        .context(MigrationSnafu { path: path.clone() })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use dhttp::home::identity::IdentityProfile;
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
