use std::sync::Arc;

use dhttp::access::db::{
    base::matcher::LocationRulesMatcher,
    service::{error::ListAllRulesError, location_service::LocationService},
};
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
    #[snafu(display("failed to load location rules from `{uri}`"))]
    LoadLocationRules {
        uri: String,
        source: ListAllRulesError,
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
    let location_rules =
        LocationService::new(&db)
            .list_all_rules()
            .await
            .context(LoadLocationRulesSnafu {
                uri: uri.to_string(),
            })?;

    Ok(PolicyBundle {
        location_rules: Arc::new(location_rules.into()),
    })
}
