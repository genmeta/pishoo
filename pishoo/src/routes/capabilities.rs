//! Read installed Lib permission groups from the configuration database.
use std::collections::{BTreeMap, BTreeSet};

use axum::{Router, response::IntoResponse};
use dhttp_home::identity::IdentityProfile;

use crate::{Error, Result};

pub(crate) fn capabilities_router(profile: IdentityProfile) -> Router {
    Router::new().route(
        "/std/acl/promissions",
        axum::routing::get(move || {
            let profile = profile.clone();
            async move {
                let result = tokio::task::spawn_blocking(move || -> Result<serde_json::Value> {
                    let path = profile.config_db_path();
                    if !path.symlink_metadata()?.is_file() {
                        return Err(Error::InvalidConfig("config.db must be a regular file".into()));
                    }
                    let mut conn = rusqlite::Connection::open_with_flags(
                        path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                    )?;
                    conn.busy_timeout(std::time::Duration::from_secs(3))?;
                    let tx = conn.transaction()?;
                    let ready: bool = tx.query_row(
                        "SELECT count(*)=2 FROM sqlite_master WHERE type='table' AND name IN ('lib_capabilities','lib_capability_operations')",
                        [], |row| row.get(0),
                    )?;
                    if !ready {
                        return Err(Error::InvalidConfig("capability registration missing; install/reinstall Libs using normal installation".into()));
                    }
                    let author: bool = tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('libs') WHERE name='author')",
                        [], |row| row.get(0),
                    )?;
                    let (key, lib_key) = if author { ("lib_name", "name") } else { ("lib_id", "id") };
                    let mut permissions = BTreeMap::<String, BTreeMap<String, BTreeSet<String>>>::new();
                    let mut statement = tx.prepare(&format!(
                        "SELECT c.title,o.path,o.method FROM lib_capabilities c JOIN libs l ON l.{lib_key}=c.{key} LEFT JOIN lib_capability_operations o ON o.{key}=c.{key} AND o.capability_id=c.capability_id ORDER BY c.title,o.path,o.method"
                    ))?;
                    for row in statement.query_map([], |row| Ok((
                        row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    )))? {
                        let (title, path, method) = row?;
                        let paths = permissions.entry(title).or_default();
                        if let (Some(path), Some(method)) = (path, method) {
                            paths.entry(path).or_default().insert(method);
                        }
                    }
                    Ok(serde_json::json!(permissions))
                }).await;
                let mut response = match result {
                    Ok(Ok(value)) => axum::Json(value).into_response(),
                    Ok(Err(error)) => (error.status(), error.to_string()).into_response(),
                    Err(error) => (http::StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
                };
                response.headers_mut().insert(
                    http::header::CACHE_CONTROL, http::HeaderValue::from_static("no-store"),
                );
                response
            }
        }),
    )
}
