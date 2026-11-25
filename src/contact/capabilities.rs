//! Capability catalog lookup, selection and conversion to daccess operations.
use dhttp_home::identity::IdentityProfile;
use http::Method;
use serde_json::{Value, json};

use super::{Failure, delivery::remote, fail};

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

/// A permission catalog advertises operations before the peer or group is known.
/// Only materialize instances that the authenticated friendship actually identifies.
pub(super) fn bind_peer(groups: &mut [Value], peer: &str) -> Result<(), Failure> {
    let peer = crate::routes::identity_segment(peer, &["list", "history"])
        .map_err(|_| fail(400, "invalid peer"))?;
    for group in groups {
        let endpoints = group["endpoints"]
            .as_array_mut()
            .ok_or_else(|| fail(502, "invalid catalog"))?;
        let named_message = endpoints
            .iter()
            .any(|op| op["path"] == "/std/comm/message/{name}" && op["method"] == "POST");
        endpoints.retain_mut(|op| {
            if named_message {
                if op["path"] == "/std/comm/message" && op["method"] == "POST" {
                    return false;
                }
                if op["path"] == "/std/comm/message/{name}" {
                    op["path"] = format!("/std/comm/message/{peer}").into();
                } else if op["path"] == "/std/comm/message/{name}/file/{file_id}"
                    && matches!(op["method"].as_str(), Some("GET" | "HEAD"))
                {
                    op["path"] = format!("/std/comm/message/{peer}/file").into();
                }
            }
            !op["path"].as_str().is_none_or(|p| p.contains(['{', '}']))
        });
    }
    Ok(())
}

/// Old applicants may copy literal templates from a new catalog. Present the
/// peer's concrete operations while preserving the original signed-in applicant
/// payload and its retry digest in daccess.
pub(super) fn incoming_request(contact: &mut Value) {
    if contact["incoming"] != true && !contact["incoming_application_id"].is_string() {
        return;
    }
    let Some(peer) = contact["name"]
        .as_str()
        .and_then(|n| crate::routes::identity_segment(n, &["list", "history"]).ok())
    else {
        return;
    };
    let Some(access) = contact
        .get_mut("requested_access")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    let named = "/std/comm/message/{name}";
    let file = "/std/comm/message/{name}/file/{file_id}";
    let receives = access
        .get(named)
        .and_then(Value::as_array)
        .is_some_and(|v| v.iter().any(|m| m == "POST"));
    if receives
        && let Some(methods) = access
            .get_mut("/std/comm/message")
            .and_then(Value::as_array_mut)
    {
        methods.retain(|m| m != "POST");
        if methods.is_empty() {
            access.remove("/std/comm/message");
        }
    }
    for (template, target, allowed) in [
        (named, format!("/std/comm/message/{peer}"), &["POST"][..]),
        (
            file,
            format!("/std/comm/message/{peer}/file"),
            &["GET", "HEAD"][..],
        ),
    ] {
        if let Some(Value::Array(methods)) = access.remove(template) {
            for method in methods
                .into_iter()
                .filter(|m| m.as_str().is_some_and(|s| allowed.contains(&s)))
            {
                let target = access.entry(target.clone()).or_insert_with(|| json!([]));
                if let Some(grants) = target.as_array_mut()
                    && !grants.contains(&method)
                {
                    grants.push(method);
                }
            }
        }
    }
    access.remove("/std/comm/group/{topic}/member/{name}");
}

pub(super) fn catalog(profile: &IdentityProfile) -> Result<Value, Failure> {
    crate::routes::permission_catalog(profile, false).map_err(|error| fail(500, error.to_string()))
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
    expand(&catalog, ids)
}
pub(super) fn expand(catalog: &Value, ids: &[Value]) -> Result<Vec<Value>, Failure> {
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
