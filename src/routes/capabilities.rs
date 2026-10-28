//! Read and replace installed Lib permission declarations in the ACL database.
use std::collections::{BTreeMap, BTreeSet};

use axum::{
    Router,
    body::Body,
    response::{IntoResponse, Response},
};
use dhttp_home::identity::IdentityProfile;
use http::{Method, Request, StatusCode, header};
use rusqlite::OptionalExtension;
use serde_json::Value;

use crate::{Error, Result};

pub(crate) fn capabilities_router(profile: IdentityProfile) -> Router {
    let write_profile = profile.clone();
    let write = move |request: Request<Body>| {
        let profile = write_profile.clone();
        async move {
            let mut response = write_request(profile, request)
                .await
                .unwrap_or_else(|error| {
                    let status = match &error {
                        Error::ConfigDatabase(rusqlite::Error::SqliteFailure(code, _))
                            if code.code == rusqlite::ErrorCode::ConstraintViolation =>
                        {
                            StatusCode::CONFLICT
                        }
                        _ => error.status(),
                    };
                    (status, error.to_string()).into_response()
                });
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
            response
        }
    };
    Router::new().route(
        "/std/acl/permissions",
        axum::routing::get(move |request: Request<Body>| {
            let profile = profile.clone();
            let details = form_urlencoded::parse(request.uri().query().unwrap_or("").as_bytes())
                .any(|(key, value)| key == "details" && value == "1");
            async move {
                let result = tokio::task::spawn_blocking(move || -> Result<serde_json::Value> {
                    let path = profile.join(".volume/sys/setting/database");
                    if !path.symlink_metadata()?.is_file() {
                        return Err(Error::InvalidConfig("config.db must be a regular file".into()));
                    }
                    let mut conn = crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
                    conn.busy_timeout(std::time::Duration::from_secs(3))?;
                    let tx = conn.transaction()?;
                    let ready: bool = tx.query_row(
                        "SELECT count(*)=2 FROM pragma_table_list WHERE type='table' AND name IN ('lib_capability','lib_capability_operation')",
                        [], |row| row.get(0),
                    )?;
                    if !ready {
                        return Err(Error::InvalidConfig("capability registration missing; install/reinstall Libs using normal installation".into()));
                    }
                    let author: bool = tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('lib') WHERE name='author')",
                        [], |row| row.get(0),
                    )?;
                    let (key, lib_key) = if author { ("lib_name", "name") } else { ("lib_id", "id") };
                    let standard = if author { "l.author='std.dhttp.net'" } else { "substr(l.id,1,14)='std.dhttp.net/'" };
                    let package = if author {
                        "CASE WHEN coalesce(l.author,'')<>'' THEN l.author||'/'||l.name ELSE l.name END"
                    } else { "l.id" };
                    let mut permissions = BTreeMap::<String, BTreeMap<String, BTreeSet<String>>>::new();
                    let mut titles = BTreeMap::<String, Value>::new();
                    let mut statement = tx.prepare(&format!(
                        "SELECT ({package}),c.title,CASE WHEN coalesce({standard},0)=0 AND substr(o.path,1,5)='/std/' THEN '/'||l.name||substr(o.path,5) ELSE o.path END,o.method FROM lib_capability c JOIN lib l ON l.{lib_key}=c.{key} LEFT JOIN lib_capability_operation o ON o.{key}=c.{key} AND o.capability_id=c.capability_id ORDER BY c.title,o.path,o.method"
                    ))?;
                    for row in statement.query_map([], |row| Ok((
                        row.get::<_, String>(0)?, row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?, row.get::<_, Option<String>>(3)?,
                    )))? {
                        let (package, title, path, method) = row?;
                        let localized = serde_json::from_str::<Value>(&title).ok()
                            .filter(|value| value["en"].is_string() && value["zh-CN"].is_string());
                        // Keep existing bilingual catalog identities stable across locales.
                        let label = localized.as_ref().map(|value| format!("{} / {}",
                            value["zh-CN"].as_str().unwrap(), value["en"].as_str().unwrap()))
                            .unwrap_or_else(|| title.clone());
                        let id = format!("{package} · {label}");
                        titles.entry(id.clone()).or_insert_with(|| localized.unwrap_or(Value::String(title)));
                        let paths = permissions.entry(id).or_default();
                        if let (Some(path), Some(method)) = (path, method) {
                            paths.entry(path).or_default().insert(method);
                        }
                    }
                    if details {
                        Ok(permissions.into_iter().map(|(id, endpoints)| {
                            let title = titles.remove(&id).unwrap();
                            (id, serde_json::json!({"title":title,"endpoints":endpoints}))
                        }).collect::<serde_json::Map<String, Value>>().into())
                    } else {
                        Ok(serde_json::json!(permissions))
                    }
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
        }).put(write.clone()).delete(write),
    )
}

async fn write_request(profile: IdentityProfile, request: Request<Body>) -> Result<Response> {
    // The outer ACL layer derives Visitor from trusted native TLS resources.
    // Public GET permission must never imply permission to rewrite declarations.
    let visitor = request
        .extensions()
        .get::<access_control::Visitor>()
        .ok_or(Error::Denied)?;
    let local = request
        .extensions()
        .get::<dhttp::HandshakeSummary>()
        .and_then(|handshake| handshake.local.as_ref())
        .or_else(|| request.extensions().get::<dhttp::LocalAuthority>())
        .ok_or(Error::MissingHandshake)?;
    let ski = dhttp_home::certificate::extract_dhttp_subject_key_identifier(local.certificates())
        .map_err(|_| Error::Denied)?;
    let subject = access_control::SubjectId::new(ski.owner_hash().as_str().as_bytes())
        .map_err(|_| Error::Denied)?;
    if visitor.name() != local.name() || visitor.subject_id() != &subject {
        return Err(Error::Denied);
    }
    let query = form_urlencoded::parse(request.uri().query().unwrap_or("").as_bytes())
        .into_owned()
        .collect::<Vec<_>>();
    if query.len() != 1 || query[0].0 != "name" || query[0].1.contains('/') {
        return Err(Error::BadRequest("exactly one Lib name is required".into()));
    }
    let name = query[0].1.clone();
    crate::package::valid_id(&name)?;
    let method = request.method().clone();
    if method == Method::PUT
        && !request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
    {
        return Ok(StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response());
    }
    let bytes = axum::body::to_bytes(
        request.into_body(),
        if method == Method::PUT {
            1024 * 1024
        } else {
            0
        },
    )
    .await
    .map_err(|_| Error::BadRequest("invalid or oversized permissions body".into()))?;
    let groups = if method == Method::PUT {
        serde_json::from_slice::<Value>(&bytes)
            .map_err(|_| Error::BadRequest("invalid permissions JSON".into()))?
    } else {
        Value::Array(Vec::new())
    };
    tokio::task::spawn_blocking(move || write_declarations(&profile, &name, &method, &groups))
        .await??;
    Ok(StatusCode::NO_CONTENT.into_response())
}

fn write_declarations(
    profile: &IdentityProfile,
    name: &str,
    method: &Method,
    groups: &Value,
) -> Result<()> {
    use std::io::Read;

    use rustix::fs::{FlockOperation, Mode, OFlags};

    // Serialize with package installation/removal before taking the same DB
    // transaction, so a write cannot restore a concurrently removed provider.
    let directory = std::fs::File::from(
        rustix::fs::open(
            profile.path(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    );
    rustix::fs::flock(&directory, FlockOperation::LockExclusive).map_err(std::io::Error::from)?;
    let mut conn = crate::setup::open_config(profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let installed: Option<(Option<String>, bool)> = tx
        .query_row(
            "SELECT author,has_app FROM package WHERE name=?1 AND has_lib=1",
            [name],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    // DELETE remains idempotent after package removal, but can still clean any
    // leftover declaration. PUT only describes an installed business Lib.
    if *method == Method::PUT && installed.is_none() {
        return Err(Error::RouteNotFound);
    }
    let groups = groups
        .as_array()
        .ok_or_else(|| Error::BadRequest("permissions must be an array".into()))?;
    let mut ids = BTreeSet::new();
    let mut endpoints = BTreeSet::new();
    let mut available = BTreeSet::new();
    if *method == Method::PUT && !groups.is_empty() {
        let standard =
            installed.as_ref().and_then(|(author, _)| author.as_deref()) == Some("std.dhttp.net");
        let prefix = if standard {
            "/std".to_owned()
        } else {
            format!("/{name}")
        };
        if installed.as_ref().is_some_and(|(_, has_app)| *has_app) {
            available.insert(("GET".to_owned(), format!("/app/{name}")));
        }
        let root = cap_std::fs::Dir::from_std_file(crate::package::lib_root(profile)?);
        let file = root.open(if standard {
            format!("std/{name}.wasm")
        } else {
            format!("{name}.wasm")
        })?;
        if !file.metadata()?.is_file() {
            return Err(Error::InvalidConfig(
                "Lib component must be a regular file".into(),
            ));
        }
        let mut bytes = Vec::new();
        file.take(64 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
        let openapi = crate::package::validate_lib(&bytes)?;
        for (path, item) in openapi.paths.as_ref().unwrap() {
            for (method, _) in item.methods() {
                available.insert((method.as_str().to_owned(), format!("{prefix}{path}")));
            }
        }
    }
    let mut rows = Vec::new();
    for group in groups {
        let object = group
            .as_object()
            .filter(|object| object.len() == 3)
            .ok_or_else(|| {
                Error::BadRequest("group requires exactly id, title and endpoints".into())
            })?;
        let id = object
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::BadRequest("invalid group id".into()))?;
        crate::package::valid_id(id)?;
        if id.contains('/') || !ids.insert(id) {
            return Err(Error::BadRequest("invalid or duplicate group id".into()));
        }
        let title = &group["title"];
        let valid_text = |value: &Value| {
            value
                .as_str()
                .is_some_and(|text| !text.trim().is_empty() && !text.chars().any(char::is_control))
        };
        if !(valid_text(title)
            || title.as_object().is_some_and(|values| {
                values.len() == 2 && valid_text(&title["en"]) && valid_text(&title["zh-CN"])
            }))
        {
            return Err(Error::BadRequest(
                "title must be text or contain en and zh-CN text".into(),
            ));
        }
        let operations = object
            .get("endpoints")
            .and_then(Value::as_array)
            .filter(|values| !values.is_empty())
            .ok_or_else(|| Error::BadRequest("group must contain endpoints".into()))?;
        let mut declared = Vec::new();
        for operation in operations {
            if operation.as_object().is_none_or(|object| object.len() != 2) {
                return Err(Error::BadRequest(
                    "endpoint requires exactly method and path".into(),
                ));
            }
            let method = operation["method"]
                .as_str()
                .ok_or_else(|| Error::BadRequest("invalid endpoint method".into()))?;
            let path = operation["path"]
                .as_str()
                .ok_or_else(|| Error::BadRequest("invalid endpoint path".into()))?;
            let endpoint = (method.to_owned(), path.to_owned());
            if !available.contains(&endpoint) || !endpoints.insert(endpoint.clone()) {
                return Err(Error::BadRequest(
                    "endpoint is unavailable or belongs to multiple groups".into(),
                ));
            }
            declared.push(endpoint);
        }
        rows.push((
            id.to_owned(),
            title
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| title.to_string()),
            declared,
        ));
    }
    permissions(&tx, name, &rows)?;
    tx.commit()?;
    Ok(())
}

/// Replace validated declarations using the caller's transaction. An empty
/// slice deletes them. Locking, commit and rollback remain with the caller.
#[allow(
    clippy::type_complexity,
    reason = "Keep the frozen transactional capability interface unchanged."
)]
pub(crate) fn permissions(
    tx: &rusqlite::Transaction<'_>,
    name: &str,
    groups: &[(String, String, Vec<(String, String)>)],
) -> Result<()> {
    tx.execute("DELETE FROM acl.lib_capability WHERE lib_name=?1", [name])?;
    for (id, title, endpoints) in groups {
        tx.execute(
            "INSERT INTO acl.lib_capability VALUES(?1,?2,?3)",
            rusqlite::params![name, id, title],
        )?;
        for (method, path) in endpoints {
            tx.execute(
                "INSERT INTO acl.lib_capability_operation VALUES(?1,?2,?3,?4)",
                rusqlite::params![name, id, method, path],
            )?;
        }
    }
    Ok(())
}
