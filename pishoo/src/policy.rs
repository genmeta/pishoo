use std::sync::Arc;

use dhttp::access::{
    db::{
        evaluator::LocationRulesDatabase,
        service::{error::EnsureStoreError, location_service::LocationService},
    },
    matcher::LocationRulesMatcher,
    policy::LocationRuleEvaluator,
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

    #[snafu(display("failed to validate access_rules database `{uri}`"))]
    ValidateDb {
        uri: String,
        source: EnsureStoreError,
    },
}

pub async fn load_policy_bundle(uri: Option<&str>) -> Result<PolicyBundle, PolicyError> {
    let Some(uri) = uri else {
        return Ok(PolicyBundle::default());
    };

    let db = sea_orm::Database::connect(uri)
        .await
        .context(ConnectDbSnafu {
            uri: uri.to_string(),
        })?;
    LocationService::new(&db)
        .ensure_store()
        .await
        .context(ValidateDbSnafu {
            uri: uri.to_string(),
        })?;

    Ok(PolicyBundle {
        location_rules: Arc::new(LocationRulesDatabase::new(db)),
    })
}

#[cfg(test)]
mod tests {
    use dhttp::access::{
        action::RequestAction,
        db::evaluator::LocationRulesDatabase,
        expr::atomics::{AtomicLocationRuleExpr, EvalError},
        policy::LocationRuleRequest,
    };
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

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

    #[tokio::test]
    async fn policy_bundle_uses_live_database_evaluator() {
        let path = std::env::temp_dir().join(format!(
            "pishoo-policy-live-db-{}.db",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let uri = format!("sqlite://{}?mode=rwc", path.display());
        let db = sea_orm::Database::connect(&uri)
            .await
            .expect("create access db");
        create_minimal_access_schema(&db).await;
        replace_root_rule(&db, 0, "*?").await;
        drop(db);

        let uri = format!("sqlite://{}?mode=rw", path.display());
        let bundle = super::load_policy_bundle(Some(&uri))
            .await
            .expect("load policy bundle");
        let request = TestRequest;

        assert_eq!(
            bundle
                .location_rules
                .evaluate("/", &request)
                .await
                .expect("policy should decide")
                .action,
            RequestAction::Allow
        );

        let db = sea_orm::Database::connect(&uri).await.expect("reopen db");
        replace_root_rule(&db, 1, "*?").await;

        assert_eq!(
            bundle
                .location_rules
                .evaluate("/", &request)
                .await
                .expect("policy should see updated DB")
                .action,
            RequestAction::Deny
        );

        let _ = std::fs::remove_file(path);
        let _type_check: Option<LocationRulesDatabase> = None;
    }
}
