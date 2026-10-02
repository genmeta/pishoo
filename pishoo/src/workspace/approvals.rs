use std::sync::Arc;

use access_control::Visitor;
use axum::{
    Extension, Json,
    extract::{Path, Query, State},
};
use http::StatusCode;
use sea_orm::{ConnectionTrait, DatabaseBackend, DbErr, Statement};
use serde::{Deserialize, Serialize};

use super::{Workspace, contacts, settings::require_owner};

type ApiError = (StatusCode, &'static str);

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ApprovalStatus {
    #[default]
    Pending,
    Expired,
}

#[derive(Default, Deserialize)]
#[serde(default)]
pub(super) struct ApprovalQuery {
    status: ApprovalStatus,
    page: Option<u64>,
    page_size: Option<u64>,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ApprovalItem {
    Capability {
        request_id: i64,
        contact_name: String,
        capability_id: &'static str,
        capability_version: &'static str,
        requested_at: i64,
        expired_after: i64,
    },
    Access {
        id: i64,
        visitor: String,
        method: String,
        api: String,
        reason: String,
        requested_at: i64,
        expired_after: i64,
    },
}

impl ApprovalItem {
    fn requested_at(&self) -> i64 {
        match self {
            Self::Capability { requested_at, .. } | Self::Access { requested_at, .. } => {
                *requested_at
            }
        }
    }

    fn expired_after(&self) -> i64 {
        match self {
            Self::Capability { expired_after, .. } | Self::Access { expired_after, .. } => {
                *expired_after
            }
        }
    }
}

#[derive(Serialize)]
pub(super) struct ApprovalPage {
    items: Vec<ApprovalItem>,
    total: usize,
    page: u64,
    page_size: u64,
}

fn access_error(error: DbErr) -> ApiError {
    tracing::error!(%error, "Workspace approval list failed");
    (StatusCode::INTERNAL_SERVER_ERROR, "Approval list failed")
}

pub(super) async fn list(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<Visitor>>,
    Query(query): Query<ApprovalQuery>,
) -> Result<Json<ApprovalPage>, ApiError> {
    require_owner(&state, visitor.as_ref().map(|extension| &extension.0))?;
    let page = query.page.unwrap_or(1);
    let page_size = query.page_size.unwrap_or(20);
    if page == 0 || !(1..=100).contains(&page_size) {
        return Err((StatusCode::BAD_REQUEST, "Invalid approval pagination"));
    }
    let expired = matches!(query.status, ApprovalStatus::Expired);
    let capability_requests = if expired {
        contacts::expired_capability_requests(&state).await?
    } else {
        contacts::pending_capability_requests(&state).await?
    };
    let mut items: Vec<ApprovalItem> = capability_requests
        .into_iter()
        .map(|request| ApprovalItem::Capability {
            request_id: request.request_id,
            contact_name: request.contact_name,
            capability_id: request.capability_id,
            capability_version: request.capability_version,
            requested_at: request.requested_at,
            expired_after: request.expired_after,
        })
        .collect();

    let condition = if expired { "<=" } else { ">" };
    let order = if expired {
        "expired_after DESC, id DESC"
    } else {
        "created_at DESC, id DESC"
    };
    let rows = state
        .access
        .database()
        .query_all_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            format!(
                "SELECT id, visitor, method, api, reason, created_at, expired_after \
                 FROM access_reviews WHERE stage = 0 AND expired_after {condition} \
                 CAST(strftime('%s', 'now') AS INTEGER) ORDER BY {order}"
            ),
        ))
        .await
        .map_err(access_error)?;
    for row in rows {
        items.push(ApprovalItem::Access {
            id: row.try_get("", "id").map_err(access_error)?,
            visitor: row.try_get("", "visitor").map_err(access_error)?,
            method: row.try_get("", "method").map_err(access_error)?,
            api: row.try_get("", "api").map_err(access_error)?,
            reason: row.try_get("", "reason").map_err(access_error)?,
            requested_at: row.try_get("", "created_at").map_err(access_error)?,
            expired_after: row.try_get("", "expired_after").map_err(access_error)?,
        });
    }
    if expired {
        items.sort_by(|left, right| right.expired_after().cmp(&left.expired_after()));
    } else {
        items.sort_by(|left, right| right.requested_at().cmp(&left.requested_at()));
    }
    let total = items.len();
    let start = page.saturating_sub(1).saturating_mul(page_size);
    let items = if start >= total as u64 {
        Vec::new()
    } else {
        items
            .into_iter()
            .skip(start as usize)
            .take(page_size as usize)
            .collect()
    };
    Ok(Json(ApprovalPage {
        items,
        total,
        page,
        page_size,
    }))
}

pub(super) async fn delete(
    State(state): State<Arc<Workspace>>,
    visitor: Option<Extension<Visitor>>,
    Path((kind, id)): Path<(String, i64)>,
) -> Result<StatusCode, ApiError> {
    require_owner(&state, visitor.as_ref().map(|extension| &extension.0))?;

    match kind.as_str() {
        "access" => {
            let deleted = state
                .access
                .database()
                .execute_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    "DELETE FROM access_reviews \
                     WHERE id = ? AND stage = 0 \
                     AND expired_after <= CAST(strftime('%s', 'now') AS INTEGER)",
                    [id.into()],
                ))
                .await
                .map_err(access_error)?;
            if deleted.rows_affected() == 0 {
                return Err((StatusCode::NOT_FOUND, "expired access approval not found"));
            }
        }
        "capability" => {
            let _guard = state.contact_write.lock().await;
            let request = contacts::expired_capability_requests(&state)
                .await?
                .into_iter()
                .find(|request| request.request_id == id)
                .ok_or((
                    StatusCode::NOT_FOUND,
                    "expired capability approval not found",
                ))?;
            state
                .access
                .delete_contacts(&[request.contact_name])
                .await
                .map_err(access_error)?;
        }
        _ => return Err((StatusCode::BAD_REQUEST, "invalid approval type")),
    }

    Ok(StatusCode::NO_CONTENT)
}
