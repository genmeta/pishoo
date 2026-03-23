use std::sync::Arc;

use firewall_db::{
    base::matcher::{DomainRulesMatcher, LocationRulesMatcher},
    service::{domain_service::DomainService, location_service::LocationService},
};
use snafu::{ResultExt, Snafu};

#[derive(Debug, Clone)]
pub struct PolicyBundle {
    pub domain_rules: Arc<DomainRulesMatcher>,
    pub location_rules: Arc<LocationRulesMatcher>,
}

impl Default for PolicyBundle {
    fn default() -> Self {
        Self {
            domain_rules: Arc::new(DomainRulesMatcher::default()),
            location_rules: Arc::new(LocationRulesMatcher::default()),
        }
    }
}

#[derive(Debug, Snafu)]
pub enum PolicyError {
    #[snafu(display("failed to connect access_rules database `{uri}`"))]
    ConnectDb { uri: String, source: sea_orm::DbErr },
    #[snafu(display("failed to load domain rules from `{uri}`"))]
    LoadDomainRules { uri: String, source: sea_orm::DbErr },
    #[snafu(display("failed to load location rules from `{uri}`"))]
    LoadLocationRules { uri: String, source: sea_orm::DbErr },
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
    let domain_rules =
        DomainService::new(&db)
            .list_all_rules()
            .await
            .context(LoadDomainRulesSnafu {
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
        domain_rules: Arc::new(domain_rules.into()),
        location_rules: Arc::new(location_rules.into()),
    })
}
