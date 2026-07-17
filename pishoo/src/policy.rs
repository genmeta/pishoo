use std::{path::PathBuf, sync::Arc};

use dhttp::{
    access::{
        db::{
            self, AccessDbError,
            evaluator::LocationRulesDatabase,
            service::{error::EnsureStoreError, location_service::LocationService},
        },
        matcher::LocationRulesMatcher,
        policy::LocationRuleEvaluator,
    },
    home::identity::IdentityProfile,
};
use snafu::{ResultExt, Snafu};

#[derive(Clone)]
pub struct PolicyBundle {
    pub location_rules: Arc<dyn LocationRuleEvaluator + Send + Sync>,
}

impl std::fmt::Debug for PolicyBundle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PolicyBundle")
            .field("location_rules", &"<location rule evaluator>")
            .finish()
    }
}

impl Default for PolicyBundle {
    fn default() -> Self {
        Self {
            location_rules: Arc::new(LocationRulesMatcher::default()),
        }
    }
}

#[derive(Debug, Snafu)]
pub enum PolicyError {
    #[snafu(display("failed to connect access_rules database `{uri}`"))]
    ConnectDb { uri: String, source: sea_orm::DbErr },

    #[snafu(display(
        "failed to open default access_rules database `{}`",
        path.display()
    ))]
    OpenDefaultDb {
        path: PathBuf,
        source: Box<AccessDbError>,
    },

    #[snafu(display(
        "failed to inspect default access_rules database `{}`",
        path.display()
    ))]
    InspectDefaultDb {
        path: PathBuf,
        source: std::io::Error,
    },

    #[snafu(display("failed to validate access_rules database `{location}`"))]
    ValidateDb {
        location: String,
        source: EnsureStoreError,
    },
}

async fn database_policy_bundle(
    db: sea_orm::DatabaseConnection,
    location: String,
) -> Result<PolicyBundle, PolicyError> {
    LocationService::new(&db)
        .ensure_store()
        .await
        .context(ValidateDbSnafu { location })?;

    Ok(PolicyBundle {
        location_rules: Arc::new(LocationRulesDatabase::new(db)),
    })
}

fn recover_missing_default_database(
    path: PathBuf,
    missing_source: AccessDbError,
    inspection: Result<(), std::io::Error>,
) -> Result<PolicyBundle, PolicyError> {
    match inspection {
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!(
                path = %path.display(),
                "default access_rules database is absent; using empty policy"
            );
            Ok(PolicyBundle::default())
        }
        Err(source) => Err(PolicyError::InspectDefaultDb { path, source }),
        Ok(()) => Err(PolicyError::OpenDefaultDb {
            path,
            source: Box::new(missing_source),
        }),
    }
}

pub async fn load_policy_bundle(
    explicit_uri: Option<&str>,
    identity_profile: Option<&IdentityProfile>,
) -> Result<PolicyBundle, PolicyError> {
    if let Some(uri) = explicit_uri {
        let db = sea_orm::Database::connect(uri)
            .await
            .context(ConnectDbSnafu {
                uri: uri.to_string(),
            })?;
        return database_policy_bundle(db, uri.to_string()).await;
    }

    let Some(identity_profile) = identity_profile else {
        return Ok(PolicyBundle::default());
    };

    let path = identity_profile.access_db_path();
    let db = match db::open_access_database(identity_profile).await {
        Ok(db) => db,
        Err(source @ AccessDbError::MissingStore { .. }) => {
            return recover_missing_default_database(
                path.clone(),
                source,
                std::fs::metadata(&path).map(|_| ()),
            );
        }
        Err(source) => {
            return Err(PolicyError::OpenDefaultDb {
                path,
                source: Box::new(source),
            });
        }
    };

    database_policy_bundle(db, path.display().to_string()).await
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use dhttp::{
        access::{
            action::RequestAction,
            db::{AccessDbError, evaluator::LocationRulesDatabase},
            expr::atomics::{AtomicLocationRuleExpr, EvalError},
            policy::LocationRuleRequest,
        },
        home::identity::IdentityProfile,
    };
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

    struct TestProfile {
        root: PathBuf,
        profile: IdentityProfile,
    }

    impl TestProfile {
        fn new(label: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "pishoo-policy-{label}-{}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("system clock should follow Unix epoch")
                    .as_nanos()
            ));
            let profile = IdentityProfile::try_from(root.join("alice.dhttp.net"))
                .expect("test profile path should contain a DHTTP name");
            std::fs::create_dir_all(profile.path()).expect("create test profile");
            Self { root, profile }
        }

        fn profile(&self) -> &IdentityProfile {
            &self.profile
        }
    }

    impl Drop for TestProfile {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    struct TestRequest;

    impl LocationRuleRequest for TestRequest {
        fn eval_atomic(&self, expr: &AtomicLocationRuleExpr) -> Result<bool, EvalError> {
            use dhttp::access::expr::eval::Evaluable;
            Ok(match expr {
                AtomicLocationRuleExpr::Any(..) => true,
                AtomicLocationRuleExpr::ClientName(pattern) => {
                    pattern.eval(&Some("alice.pilot.dhttp.net"))?
                }
                AtomicLocationRuleExpr::Method(_) => false,
                AtomicLocationRuleExpr::Header(_) => false,
                AtomicLocationRuleExpr::Query(_) => false,
            })
        }
    }

    async fn create_minimal_access_schema(db: &sea_orm::DatabaseConnection) {
        db.execute(Statement::from_string(
            DatabaseBackend::Sqlite,
            "CREATE TABLE location_rule_sets (id INTEGER PRIMARY KEY AUTOINCREMENT, pattern JSON NOT NULL UNIQUE, created_at TEXT NOT NULL, updated_at TEXT NOT NULL)".to_owned(),
        ))
        .await
        .expect("create location rule sets");
        db.execute(Statement::from_string(
            DatabaseBackend::Sqlite,
            "CREATE TABLE location_rules (id INTEGER PRIMARY KEY AUTOINCREMENT, location_id INTEGER NOT NULL, action INTEGER NOT NULL, exprs JSON NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, FOREIGN KEY(location_id) REFERENCES location_rule_sets(id) ON DELETE CASCADE)".to_owned(),
        ))
        .await
        .expect("create location rules");
        db.execute(Statement::from_string(
            DatabaseBackend::Sqlite,
            "INSERT INTO location_rule_sets (id, pattern, created_at, updated_at) VALUES (1, '\"/\"', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')".to_owned(),
        ))
        .await
        .expect("insert root location");
    }

    async fn replace_root_rule(db: &sea_orm::DatabaseConnection, action: i32, expr: &str) {
        db.execute(Statement::from_string(
            DatabaseBackend::Sqlite,
            "DELETE FROM location_rules WHERE location_id = 1".to_owned(),
        ))
        .await
        .expect("clear root rules");
        let expr_json = serde_json::json!({
            "infix": expr,
            "polish": format!("{expr} "),
        })
        .to_string()
        .replace('\'', "''");
        db.execute(Statement::from_string(
            DatabaseBackend::Sqlite,
            format!(
                "INSERT INTO location_rules (location_id, action, exprs, created_at, updated_at) VALUES (1, {action}, '{expr_json}', '2026-01-01T00:00:01Z', '2026-01-01T00:00:01Z')"
            ),
        ))
        .await
        .expect("insert root rule");
    }

    async fn create_access_database(path: &Path, action: i32) -> String {
        std::fs::create_dir_all(path.parent().expect("access DB path should have a parent"))
            .expect("create access DB parent");
        let create_uri = format!("sqlite://{}?mode=rwc", path.display());
        let db = sea_orm::Database::connect(&create_uri)
            .await
            .expect("create access DB");
        create_minimal_access_schema(&db).await;
        replace_root_rule(&db, action, "*?").await;
        drop(db);
        format!("sqlite://{}?mode=rw", path.display())
    }

    async fn evaluated_action(bundle: &super::PolicyBundle) -> RequestAction {
        bundle
            .location_rules
            .evaluate("/", &TestRequest)
            .await
            .expect("policy should decide")
            .action
    }

    #[tokio::test]
    async fn identity_default_policy_is_loaded_and_remains_live() {
        let test_profile = TestProfile::new("profile-live");
        let path = test_profile.profile().access_db_path();
        let uri = create_access_database(&path, 0).await;

        let bundle = super::load_policy_bundle(None, Some(test_profile.profile()))
            .await
            .expect("profile default access DB should load");
        assert_eq!(evaluated_action(&bundle).await, RequestAction::Allow);

        let db = sea_orm::Database::connect(&uri)
            .await
            .expect("reopen profile default access DB");
        replace_root_rule(&db, 1, "*?").await;

        assert_eq!(evaluated_action(&bundle).await, RequestAction::Deny);
        let _type_check: Option<LocationRulesDatabase> = None;
    }

    #[tokio::test]
    async fn missing_identity_default_policy_falls_back_to_empty() {
        let test_profile = TestProfile::new("profile-missing");

        let bundle = super::load_policy_bundle(None, Some(test_profile.profile()))
            .await
            .expect("missing implicit access DB should be recoverable");

        assert!(
            bundle
                .location_rules
                .evaluate("/", &TestRequest)
                .await
                .is_err(),
            "empty policy must not synthesize a root rule"
        );
    }

    #[tokio::test]
    async fn identity_default_directory_is_not_recoverable() {
        let test_profile = TestProfile::new("profile-directory");
        std::fs::create_dir_all(test_profile.profile().access_db_path())
            .expect("create directory at access DB path");

        let error = super::load_policy_bundle(None, Some(test_profile.profile()))
            .await
            .expect_err("an existing non-file access path must fail");

        assert!(matches!(error, super::PolicyError::OpenDefaultDb { .. }));
    }

    #[test]
    fn identity_default_metadata_permission_error_is_not_recoverable() {
        let path = PathBuf::from("/profile/db/access.db");
        let missing = AccessDbError::MissingStore { path: path.clone() };

        let error = super::recover_missing_default_database(
            path,
            missing,
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied")),
        )
        .expect_err("metadata permission errors must fail");

        assert!(matches!(
            error,
            super::PolicyError::InspectDefaultDb { source, .. }
                if source.kind() == io::ErrorKind::PermissionDenied
        ));
    }

    #[tokio::test]
    async fn corrupt_identity_default_database_is_not_recoverable() {
        let test_profile = TestProfile::new("profile-corrupt");
        let path = test_profile.profile().access_db_path();
        std::fs::create_dir_all(path.parent().expect("access DB path should have a parent"))
            .expect("create access DB parent");
        std::fs::write(&path, b"not sqlite").expect("write corrupt access DB");

        let error = super::load_policy_bundle(None, Some(test_profile.profile()))
            .await
            .expect_err("corrupt profile access DB must fail");

        assert!(matches!(error, super::PolicyError::OpenDefaultDb { .. }));
    }

    #[tokio::test]
    async fn identity_default_database_without_schema_is_not_recoverable() {
        let test_profile = TestProfile::new("profile-schema");
        let path = test_profile.profile().access_db_path();
        std::fs::create_dir_all(path.parent().expect("access DB path should have a parent"))
            .expect("create access DB parent");
        let uri = format!("sqlite://{}?mode=rwc", path.display());
        drop(
            sea_orm::Database::connect(&uri)
                .await
                .expect("create schema-less SQLite DB"),
        );

        let error = super::load_policy_bundle(None, Some(test_profile.profile()))
            .await
            .expect_err("schema-less profile access DB must fail validation");

        assert!(matches!(error, super::PolicyError::ValidateDb { .. }));
    }

    #[tokio::test]
    async fn invalid_explicit_uri_never_falls_back_to_valid_profile_default() {
        let test_profile = TestProfile::new("explicit-invalid");
        create_access_database(&test_profile.profile().access_db_path(), 0).await;
        let missing_uri = format!(
            "sqlite://{}?mode=rw",
            test_profile.root.join("explicit-missing.db").display()
        );

        let error = super::load_policy_bundle(Some(&missing_uri), Some(test_profile.profile()))
            .await
            .expect_err("explicit URI failure must terminate selection");

        assert!(matches!(error, super::PolicyError::ConnectDb { .. }));
    }

    #[tokio::test]
    async fn valid_explicit_uri_wins_over_valid_profile_default() {
        let test_profile = TestProfile::new("explicit-wins");
        create_access_database(&test_profile.profile().access_db_path(), 0).await;
        let explicit_uri = create_access_database(&test_profile.root.join("explicit.db"), 1).await;

        let bundle = super::load_policy_bundle(Some(&explicit_uri), Some(test_profile.profile()))
            .await
            .expect("explicit access DB should load");

        assert_eq!(evaluated_action(&bundle).await, RequestAction::Deny);
    }

    #[tokio::test]
    async fn direct_server_without_explicit_uri_keeps_empty_policy() {
        let bundle = super::load_policy_bundle(None, None)
            .await
            .expect("direct server without access_rules should keep empty policy");

        assert!(
            bundle
                .location_rules
                .evaluate("/", &TestRequest)
                .await
                .is_err(),
            "direct server must not synthesize profile rules"
        );
    }
}
