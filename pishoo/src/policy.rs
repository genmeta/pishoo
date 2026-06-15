use std::sync::Arc;

use dhttp::access::{
    action::RequestAction,
    expr::exprs::LocationRuleExprs,
    matcher::{LocationRulesMatcher, PatternWithTime},
    pattern::{LocationPattern, LocationPatternKind},
};
use sea_orm::{ConnectionTrait, DatabaseBackend, QueryResult, Statement};
use serde::de::DeserializeOwned;
use snafu::{ResultExt, Snafu};

#[derive(Debug, Clone)]
pub struct PolicyBundle {
    pub location_rules: Arc<LocationRulesMatcher>,
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

    #[snafu(display("failed to query access rule locations from `{uri}`"))]
    QueryLocations { uri: String, source: sea_orm::DbErr },

    #[snafu(display("failed to query access rules from `{uri}`"))]
    QueryRules { uri: String, source: sea_orm::DbErr },

    #[snafu(display("failed to read access rule location id from `{uri}`"))]
    ReadLocationId { uri: String, source: sea_orm::DbErr },

    #[snafu(display("failed to read access rule location pattern from `{uri}`"))]
    ReadLocationPattern {
        uri: String,
        location_id: i32,
        source: sea_orm::DbErr,
    },

    #[snafu(display("failed to decode access rule location pattern from `{uri}`"))]
    DecodeLocationPattern {
        uri: String,
        location_id: i32,
        source: serde_json::Error,
    },

    #[snafu(display("failed to read access rule id from `{uri}`"))]
    ReadRuleId {
        uri: String,
        location_id: i32,
        source: sea_orm::DbErr,
    },

    #[snafu(display("failed to read access rule action from `{uri}`"))]
    ReadRuleAction {
        uri: String,
        rule_id: i32,
        source: sea_orm::DbErr,
    },

    #[snafu(display("unexpected access rule action {action} in `{uri}`"))]
    InvalidRuleAction {
        uri: String,
        rule_id: i32,
        action: i32,
    },

    #[snafu(display("failed to read access rule expression from `{uri}`"))]
    ReadRuleExprs {
        uri: String,
        rule_id: i32,
        source: sea_orm::DbErr,
    },

    #[snafu(display("failed to decode access rule expression from `{uri}`"))]
    DecodeRuleExprs {
        uri: String,
        rule_id: i32,
        source: serde_json::Error,
    },
}

fn decode_json<T>(value: serde_json::Value) -> Result<T, serde_json::Error>
where
    T: DeserializeOwned,
{
    serde_json::from_value(value)
}

fn location_id(row: &QueryResult, uri: &str) -> Result<i32, PolicyError> {
    row.try_get("", "id").context(ReadLocationIdSnafu {
        uri: uri.to_string(),
    })
}

fn location_pattern(
    row: &QueryResult,
    uri: &str,
    location_id: i32,
) -> Result<LocationPattern, PolicyError> {
    let value = row
        .try_get("", "pattern")
        .context(ReadLocationPatternSnafu {
            uri: uri.to_string(),
            location_id,
        })?;
    decode_json(value).context(DecodeLocationPatternSnafu {
        uri: uri.to_string(),
        location_id,
    })
}

fn rule_id(row: &QueryResult, uri: &str, location_id: i32) -> Result<i32, PolicyError> {
    row.try_get("", "id").context(ReadRuleIdSnafu {
        uri: uri.to_string(),
        location_id,
    })
}

fn rule_action(row: &QueryResult, uri: &str, rule_id: i32) -> Result<RequestAction, PolicyError> {
    let action = row.try_get("", "action").context(ReadRuleActionSnafu {
        uri: uri.to_string(),
        rule_id,
    })?;
    match action {
        0 => Ok(RequestAction::Allow),
        1 => Ok(RequestAction::Deny),
        action => InvalidRuleActionSnafu {
            uri: uri.to_string(),
            rule_id,
            action,
        }
        .fail(),
    }
}

fn rule_exprs(
    row: &QueryResult,
    uri: &str,
    rule_id: i32,
) -> Result<LocationRuleExprs, PolicyError> {
    let value = row.try_get("", "exprs").context(ReadRuleExprsSnafu {
        uri: uri.to_string(),
        rule_id,
    })?;
    decode_json(value).context(DecodeRuleExprsSnafu {
        uri: uri.to_string(),
        rule_id,
    })
}

async fn load_location_rules(
    database: &sea_orm::DatabaseConnection,
    uri: &str,
) -> Result<LocationRulesMatcher, PolicyError> {
    let locations = database
        .query_all(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT id, pattern FROM location_rule_sets".to_string(),
        ))
        .await
        .context(QueryLocationsSnafu {
            uri: uri.to_string(),
        })?;

    let mut matcher = LocationRulesMatcher::default();
    for location in locations {
        let location_id = location_id(&location, uri)?;
        let pattern = location_pattern(&location, uri, location_id)?;
        let rules = database
            .query_all(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT id, action, exprs FROM location_rules WHERE location_id = ? ORDER BY created_at ASC",
                [location_id.into()],
            ))
            .await
            .context(QueryRulesSnafu {
                uri: uri.to_string(),
            })?;

        let rules = rules
            .into_iter()
            .map(|row| {
                let rule_id = rule_id(&row, uri, location_id)?;
                Ok((
                    rule_exprs(&row, uri, rule_id)?,
                    rule_action(&row, uri, rule_id)?,
                ))
            })
            .collect::<Result<Vec<_>, PolicyError>>()?;

        matcher.map.insert(
            PatternWithTime::<LocationPatternKind>::new(location_id.into(), pattern),
            rules,
        );
    }

    Ok(matcher)
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
    let location_rules = load_location_rules(&db, uri).await?;

    Ok(PolicyBundle {
        location_rules: Arc::new(location_rules),
    })
}
