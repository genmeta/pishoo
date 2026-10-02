use std::sync::Arc;

use access_control::Visitor;
use axum::{Extension, Json, extract::State};
use http::StatusCode;
use sea_orm::{ConnectionTrait, DatabaseBackend, DbErr, Statement, TransactionTrait};
use serde::{Deserialize, Serialize};

use super::Workspace;

pub(super) type ApiError = (StatusCode, &'static str);

#[derive(Serialize)]
pub(crate) struct ProfileSettings {
    identity_name: String,
    display_name: Option<String>,
    avatar_url: Option<&'static str>,
    updated_at: i64,
}

pub(super) struct StoredProfile {
    pub display_name: Option<String>,
    pub avatar_name: Option<String>,
    pub updated_at: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfilePatch {
    /// An empty string clears the local display name.
    display_name: String,
}

pub(super) fn storage_error(error: DbErr) -> ApiError {
    tracing::error!(error = %error, "Workspace settings storage failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "Workspace storage failed",
    )
}

pub(super) fn require_owner(state: &Workspace, visitor: Option<&Visitor>) -> Result<(), ApiError> {
    state
        .owner
        .ensure(visitor)
        .map_err(|status| (status, "profile owner required"))
}

pub(super) async fn read_stored_profile(
    db: &impl ConnectionTrait,
) -> Result<StoredProfile, ApiError> {
    let row = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT display_name, avatar_name, updated_at FROM profile_preferences WHERE id = 1"
                .to_owned(),
        ))
        .await
        .map_err(storage_error)?
        .ok_or_else(|| storage_error(DbErr::RecordNotFound("profile preferences".to_owned())))?;
    Ok(StoredProfile {
        display_name: row.try_get("", "display_name").map_err(storage_error)?,
        avatar_name: row.try_get("", "avatar_name").map_err(storage_error)?,
        updated_at: row.try_get("", "updated_at").map_err(storage_error)?,
    })
}

pub(super) fn profile_settings(identity_name: &str, stored: StoredProfile) -> ProfileSettings {
    ProfileSettings {
        identity_name: identity_name.to_owned(),
        display_name: stored.display_name,
        avatar_url: stored
            .avatar_name
            .as_ref()
            .map(|_| "/workspace-api/settings/profile/avatar"),
        updated_at: stored.updated_at,
    }
}

pub async fn get_profile(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<Visitor>>,
) -> Result<Json<ProfileSettings>, ApiError> {
    require_owner(&state, visitor.as_ref().map(|extension| &extension.0))?;
    let stored = read_stored_profile(state.store.db()).await?;
    Ok(Json(profile_settings(state.owner.name(), stored)))
}

pub async fn patch_profile(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<Visitor>>,
    Json(body): Json<ProfilePatch>,
) -> Result<Json<ProfileSettings>, ApiError> {
    require_owner(&state, visitor.as_ref().map(|extension| &extension.0))?;
    let name = body.display_name.trim();
    if name.chars().count() > 80 || name.chars().any(char::is_control) {
        return Err((StatusCode::BAD_REQUEST, "invalid display name"));
    }
    let name = if name.is_empty() {
        None
    } else {
        Some(name.to_owned())
    };
    let transaction = state.store.db().begin().await.map_err(storage_error)?;
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE profile_preferences SET display_name = ?, updated_at = MAX(CAST(strftime('%s', 'now') AS INTEGER), updated_at + 1) WHERE id = 1",
            [name.into()],
        ))
        .await
        .map_err(storage_error)?;
    let profile = profile_settings(state.owner.name(), read_stored_profile(&transaction).await?);
    transaction.commit().await.map_err(storage_error)?;
    Ok(Json(profile))
}
