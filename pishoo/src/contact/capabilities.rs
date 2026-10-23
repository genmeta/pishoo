//! Capability catalog lookup, selection and conversion to daccess operations.
use dhttp_home::identity::IdentityProfile;
use http::Method;
use serde_json::{Value, json};

use super::{Failure, delivery::remote, fail, storage::storage};

pub(super) fn requested(selected: &[Value]) -> Value {
    let mut access = json!({});
    for capability in selected {
        for endpoint in capability["endpoints"].as_array().unwrap() {
            let path = endpoint["path"].as_str().unwrap();
            if access.get(path).is_none() {
                access[path] = json!([]);
            }
            if !access[path]
                .as_array()
                .unwrap()
                .contains(&endpoint["method"])
            {
                access[path]
                    .as_array_mut()
                    .unwrap()
                    .push(endpoint["method"].clone());
            }
        }
    }
    access
}
pub(super) fn offered(selected: &[Value]) -> Value {
    let mut access = requested(selected);
    for methods in access.as_object_mut().unwrap().values_mut() {
        *methods = json!({"allow":methods.clone(),"review":[],"deny":[]});
    }
    access
}

pub(super) fn catalog(profile: &IdentityProfile) -> Result<Value, Failure> {
    let db = crate::setup::open_config(&profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| fail(500, error.to_string()))?;
    let author: bool = db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('lib') WHERE name='author')",
            [],
            |row| row.get(0),
        )
        .map_err(storage)?;
    let (key, lib_key) = if author {
        ("lib_name", "name")
    } else {
        ("lib_id", "id")
    };
    let standard = if author {
        "l.author='std.dhttp.net'"
    } else {
        "substr(l.id,1,14)='std.dhttp.net/'"
    };
    let package = if author {
        "CASE WHEN coalesce(l.author,'')<>'' THEN l.author||'/'||l.name ELSE l.name END"
    } else {
        "l.id"
    };
    let mut permissions = std::collections::BTreeMap::<
        String,
        std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    >::new();
    let mut stmt = db.prepare(&format!(
        "SELECT ({package}),c.title,CASE WHEN coalesce({standard},0)=0 AND substr(o.path,1,5)='/std/' THEN '/'||l.name||substr(o.path,5) ELSE o.path END,o.method FROM lib_capability c JOIN lib l ON l.{lib_key}=c.{key} LEFT JOIN lib_capability_operation o ON o.{key}=c.{key} AND o.capability_id=c.capability_id ORDER BY c.title,o.path,o.method"
    )).map_err(storage)?;
    for row in stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(storage)?
    {
        let (package, title, path, method) = row.map_err(storage)?;
        let localized = serde_json::from_str::<Value>(&title)
            .ok()
            .filter(|value| value["en"].is_string() && value["zh-CN"].is_string());
        let label = localized
            .as_ref()
            .map(|value| {
                format!(
                    "{} / {}",
                    value["zh-CN"].as_str().unwrap(),
                    value["en"].as_str().unwrap()
                )
            })
            .unwrap_or(title);
        let paths = permissions
            .entry(format!("{package} · {label}"))
            .or_default();
        if let (Some(path), Some(method)) = (path, method) {
            paths.entry(path).or_default().insert(method);
        }
    }
    Ok(json!(permissions))
}
pub(super) async fn selected(
    profile: &IdentityProfile,
    endpoint: &dhttp::Endpoint,
    ids: &Value,
    target: &str,
) -> Result<Vec<Value>, Failure> {
    let ids = ids
        .as_array()
        .ok_or_else(|| fail(400, "invalid capabilities"))?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let catalog = if target == endpoint.name() {
        let profile = profile.clone();
        tokio::task::spawn_blocking(move || catalog(&profile))
            .await
            .map_err(|e| fail(500, e.to_string()))??
    } else {
        remote(
            endpoint,
            target,
            Method::GET,
            "/std/acl/permissions",
            None,
            None,
        )
        .await?
        .1
    };
    let catalog = catalog
        .as_object()
        .ok_or_else(|| fail(502, "invalid permission catalog"))?;
    let mut result = Vec::new();
    for id in ids {
        let id = id
            .as_str()
            .ok_or_else(|| fail(400, "invalid permission name"))?;
        // Existing queued requests may still contain an unqualified title.
        // Resolve it only when exactly one installed package owns that title.
        let (id, paths) = if let Some(entry) = catalog.get_key_value(id) {
            entry
        } else {
            let mut matches = catalog
                .iter()
                .filter(|(name, _)| name.split_once(" · ").is_some_and(|(_, title)| title == id));
            let entry = matches
                .next()
                .ok_or_else(|| fail(409, "selected capability unavailable"))?;
            if matches.next().is_some() {
                return Err(fail(
                    409,
                    "selected capability is ambiguous; refresh the catalog",
                ));
            }
            entry
        };
        if result
            .iter()
            .any(|value: &Value| value["id"] == id.as_str())
        {
            return Err(fail(400, "duplicate capability"));
        }
        let paths = paths
            .as_object()
            .ok_or_else(|| fail(502, "invalid permission paths"))?;
        let mut endpoints = Vec::new();
        for (path, methods) in paths {
            if !path.starts_with('/')
                || path.len() == 1
                || path.starts_with("//")
                || path.contains(['?', '#', '%', '\\'])
                || path.split('/').any(|part| matches!(part, "." | ".."))
                || path.parse::<http::Uri>().is_err()
            {
                return Err(fail(502, "invalid permission path"));
            }
            for method in methods
                .as_array()
                .ok_or_else(|| fail(502, "invalid permission methods"))?
            {
                let method = method
                    .as_str()
                    .ok_or_else(|| fail(502, "invalid permission method"))?;
                if method != method.to_ascii_uppercase()
                    || Method::from_bytes(method.as_bytes()).is_err()
                {
                    return Err(fail(502, "invalid permission method"));
                }
                endpoints.push(json!({"method":method,"path":path}));
            }
        }
        result.push(json!({"id":id,"endpoints":endpoints}));
    }
    Ok(result)
}
