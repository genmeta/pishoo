//! Native contact collections and outbound delivery; daccess owns relationships and ACL.
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use access_control::AccessService;
use axum::{Router, body::Body, response::IntoResponse};
use dhttp_home::identity::IdentityProfile;
use http::{Method, Request, StatusCode};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use tower::ServiceExt;

mod access;
mod capabilities;
mod decisions;
mod delivery;
mod directory;
mod listing;
mod storage;

pub(crate) use delivery::contact_jobs;
pub(crate) use storage::database;

use self::{
    access::contact,
    capabilities::selected,
    delivery::remote,
    storage::{expire_requests, outbound_request, storage},
};

type Failure = (u16, String);

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
fn random_id() -> Result<String, Failure> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| fail(500, e.to_string()))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
fn fail(status: u16, message: impl Into<String>) -> Failure {
    (status, message.into())
}

fn name(raw: &str) -> Result<String, Failure> {
    let raw = raw.trim().to_ascii_lowercase();
    let expanded = if raw.ends_with('~') {
        format!(
            "{}.dhttp.net",
            raw.trim_end_matches('~').trim_end_matches('.')
        )
    } else if raw.ends_with(".dhttp.net") {
        raw
    } else {
        format!("{raw}.dhttp.net")
    };
    if expanded.len() > 253
        || expanded.trim_end_matches(".dhttp.net").is_empty()
        || !expanded.split('.').all(|s| {
            !s.is_empty()
                && s.len() <= 63
                && !s.starts_with('-')
                && !s.ends_with('-')
                && s.bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        })
    {
        return Err(fail(400, "invalid identity name"));
    }
    Ok(expanded)
}
fn parameter(target: &str, key: &str) -> Result<Option<String>, Failure> {
    let mut result = None;
    for (k, v) in form_urlencoded::parse(target.split_once('?').map_or("", |(_, q)| q).as_bytes()) {
        if k == key {
            if result.is_some() {
                return Err(fail(400, "duplicate parameter"));
            }
            result = Some(v.into_owned());
        }
    }
    Ok(result)
}

fn integer(target: &str, key: &str, default: i64) -> Result<i64, Failure> {
    parameter(target, key)?.map_or(Ok(default), |v| {
        v.parse().map_err(|_| fail(400, "invalid number"))
    })
}
fn page(target: &str) -> Result<(i64, i64), Failure> {
    let p = integer(target, "page", 1)?;
    let size = integer(target, "page_size", 20)?;
    if p < 1
        || !(1..=100).contains(&size)
        || p.checked_sub(1).and_then(|p| p.checked_mul(size)).is_none()
    {
        return Err(fail(400, "invalid pagination"));
    }
    Ok((p, size))
}
fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, Failure> {
    value[key]
        .as_str()
        .ok_or_else(|| fail(400, format!("invalid {key}")))
}

fn project_contact(mut contact: Value, draft: &Value) -> Value {
    if !contact.is_null()
        && (draft["_contact_id"] != contact["id"]
            || draft["_contact_subject"] != contact["subject_id"])
    {
        return contact;
    }
    if contact.is_null() {
        contact = json!({"kind":"name","id":null,"name":draft["target_name"],"subject_id":null,"alias":null,"class":"Human",
            "description":draft["description"],"status":"pending","incoming":false,
            "created_at":draft["created_at"],"updated_at":draft["updated_at"],"valid_before":draft["valid_before"],
            "requested_access":{},"granted_access":{},"offers":{}});
    }
    contact["updated_at"] = contact["updated_at"]
        .as_i64()
        .unwrap_or(0)
        .max(draft["updated_at"].as_i64().unwrap_or(0))
        .into();
    contact["application_id"] = draft["application_id"].clone();
    contact["delivery_status"] = draft["status"].clone();
    contact["delivery_error"] = draft["error_message"].clone();
    contact
}

pub(crate) fn contact_router(
    contact_db: Arc<std::sync::Mutex<rusqlite::Connection>>,
    profile: IdentityProfile,
    endpoint: dhttp::Endpoint,
    access: Arc<AccessService>,
) -> Router {
    crate::routes::access_router(access.clone())
        .route(
            "/std/contact/{name}",
            axum::routing::any(|| async { StatusCode::METHOD_NOT_ALLOWED }),
        )
        .route(
            "/std/contact/requests/{id}",
            axum::routing::any(|| async { StatusCode::METHOD_NOT_ALLOWED }),
        )
        .layer(axum::middleware::from_fn(
            move |request: Request<Body>, next: axum::middleware::Next| {
                let contact_db = contact_db.clone();
                let (profile, endpoint, access) =
                    (profile.clone(), endpoint.clone(), access.clone());
                async move {
                    let path = request.uri().path();
                    if !((path == "/std/contact"
                        && matches!(
                            *request.method(),
                            Method::GET
                                | Method::POST
                                | Method::PUT
                                | Method::PATCH
                                | Method::DELETE
                        ))
                        || path.starts_with("/std/contact/"))
                    {
                        return next.run(request).await;
                    }
                    let group_resource = path.starts_with("/std/contact/") && path.contains("~%23");
                    let result = dispatch(contact_db, profile, endpoint, access, request).await;
                    let mut response = match result {
                        Ok((204, _)) => StatusCode::NO_CONTENT.into_response(),
                        Ok((status, value)) => {
                            let tag = group_resource.then(|| directory::etag(&value)).flatten();
                            let mut response =
                                (StatusCode::from_u16(status).unwrap(), axum::Json(value))
                                    .into_response();
                            if let Some(tag) = tag {
                                response
                                    .headers_mut()
                                    .insert(http::header::ETAG, tag.parse().unwrap());
                            }
                            response
                        }
                        Err((status, message)) => (
                            StatusCode::from_u16(status)
                                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                            axum::Json(json!({"error":message})),
                        )
                            .into_response(),
                    };
                    response
                        .headers_mut()
                        .insert(http::header::CACHE_CONTROL, "no-store".parse().unwrap());
                    response
                }
            },
        ))
}

async fn dispatch(
    contact_db: Arc<std::sync::Mutex<rusqlite::Connection>>,
    profile: IdentityProfile,
    endpoint: dhttp::Endpoint,
    access: Arc<AccessService>,
    request: Request<Body>,
) -> Result<(u16, Value), Failure> {
    let visitor = request
        .extensions()
        .get::<access_control::Visitor>()
        .cloned();
    let verified_owner = access::owner(&endpoint)?;
    let service = request
        .extensions()
        .get::<Arc<crate::sandbox::Lib>>()
        .map(|lib| {
            let subject = lib.author_owner_hash.clone().unwrap_or_else(|| {
                String::from_utf8_lossy(verified_owner.subject_id().as_bytes()).into_owned()
            });
            (lib.id.clone(), subject)
        });
    let owner = visitor.as_ref().is_some_and(|v| {
        v.name() == verified_owner.name() && v.subject_id() == verified_owner.subject_id()
    });
    let method = request.method().clone();
    if matches!(
        method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    ) && request
        .headers()
        .get("sec-fetch-site")
        .is_some_and(|v| v == "cross-site")
    {
        return Err(fail(403, "cross-origin request denied"));
    }
    let mut target = request.uri().path_and_query().unwrap().as_str().to_owned();
    let mut path = request.uri().path().to_owned();
    let if_match = request
        .headers()
        .get("if-match")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let request_query = request.uri().query().unwrap_or("").to_owned();
    let mut bytes = axum::body::to_bytes(
        request.into_body(),
        if matches!(
            method,
            Method::POST | Method::PUT | Method::PATCH | Method::DELETE
        ) {
            65536
        } else {
            0
        },
    )
    .await
    .map_err(|_| fail(400, "invalid request body"))?;
    let resource = path
        .strip_prefix("/std/contact/")
        .filter(|p| *p != "list")
        .map(str::to_owned);
    if let Some(resource) = &resource {
        if !request_query.is_empty() {
            return Err(fail(400, "unexpected contact resource query"));
        }
        if let Some(id) = resource.strip_prefix("requests/") {
            if !decisions::valid_id(id) {
                return Err(fail(400, "invalid application_id"));
            }
            let person = visitor
                .as_ref()
                .ok_or_else(|| fail(403, "verified visitor required"))?;
            return match method {
                Method::GET => Ok((
                    200,
                    access::receipt(&access, &verified_owner, person, id).await?,
                )),
                Method::PATCH if owner && service.is_none() => {
                    let body: Value =
                        serde_json::from_slice(&bytes).map_err(|_| fail(400, "invalid JSON"))?;
                    let applicant = decisions::decide(&access, id, &body).await?;
                    Ok((
                        200,
                        access::receipt(&access, &verified_owner, &applicant, id).await?,
                    ))
                }
                Method::PATCH => Err(fail(403, "direct owner required")),
                _ => Err(fail(405, "method not allowed")),
            };
        }
        if resource.contains('/') || resource == "requests" {
            return Err(fail(404, "contact not found"));
        }
        let person = if resource.contains("~%23") {
            directory::group_name(&resource.replace("~%23", "~#"))?
        } else {
            name(resource)?
        };
        target = format!("/std/contact?name={}", access::encoded(&person));
        path = "/std/contact".to_owned();
        if person.contains("~#") {
            if method == Method::PUT {
                let mut doc: Value =
                    serde_json::from_slice(&bytes).map_err(|_| fail(400, "invalid JSON"))?;
                if doc
                    .as_object()
                    .is_none_or(|o| o.len() != 1 || !o.contains_key("bound_subject"))
                {
                    return Err(fail(400, "expected bound_subject"));
                }
                doc["kind"] = "group".into();
                doc["name"] = person.into();
                return Ok((
                    200,
                    directory::put(&access, &doc, service.as_ref(), now()).await?,
                ));
            }
            if method == Method::DELETE {
                if !service
                    .as_ref()
                    .is_some_and(|s| directory::is_group_provider(&s.0))
                {
                    return Err(fail(403, "local_group_required"));
                }
                directory::remove(&access, &person, if_match.as_deref()).await?;
                return Ok((204, Value::Null));
            }
        } else if method == Method::POST {
            let mut doc: Value =
                serde_json::from_slice(&bytes).map_err(|_| fail(400, "invalid JSON"))?;
            let fields = doc
                .as_object_mut()
                .ok_or_else(|| fail(400, "invalid application"))?;
            if fields.contains_key("from") || fields.contains_key("to") {
                return Err(fail(400, "application identities come from the path"));
            }
            if owner && service.is_none() {
                fields.insert("from".into(), endpoint.name().into());
                fields.insert("to".into(), person.into());
                fields
                    .entry("requested_capabilities")
                    .or_insert_with(|| json!([]));
                fields
                    .entry("offered_capabilities")
                    .or_insert_with(|| json!([]));
            } else {
                if service.is_some() || visitor.as_ref().is_none_or(|v| v.name() != person) {
                    return Err(fail(403, "applicant does not match the verified caller"));
                }
                fields.insert("from".into(), person.into());
                fields.insert("to".into(), endpoint.name().into());
                fields.entry("class").or_insert_with(|| json!("Human"));
            }
            bytes = serde_json::to_vec(&doc)
                .map_err(|_| fail(400, "invalid application"))?
                .into();
        } else if method == Method::PUT {
            return Err(fail(405, "method not allowed"));
        }
    }
    if path == "/std/contact/list" && method != Method::GET {
        return Err(fail(405, "method not allowed"));
    }
    let native = |method: Method, uri: String, body: Option<Value>| {
        let access = access.clone();
        let visitor = visitor.clone();
        async move {
            let mut request = Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.map_or_else(Vec::new, |value| {
                    serde_json::to_vec(&value).unwrap()
                })))
                .map_err(|error| fail(500, error.to_string()))?;
            if let Some(visitor) = visitor {
                request.extensions_mut().insert(visitor);
            }
            let response = crate::routes::access_router(access)
                .oneshot(request)
                .await
                .unwrap();
            let status = response.status().as_u16();
            let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .map_err(|error| fail(500, error.to_string()))?;
            if !(200..300).contains(&status) {
                return Err(fail(status, String::from_utf8_lossy(&bytes).into_owned()));
            }
            Ok((
                status,
                if bytes.is_empty() {
                    Value::Null
                } else {
                    serde_json::from_slice(&bytes)
                        .map_err(|_| fail(500, "invalid contact response"))?
                },
            ))
        }
    };
    if method == Method::GET
        && path == "/std/contact"
        && let Some(id) = parameter(&target, "application_id")?
    {
        if form_urlencoded::parse(request_query.as_bytes()).count() != 1
            || id.len() != 64
            || !id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(fail(400, "invalid application_id"));
        }
        let applicant = visitor
            .as_ref()
            .ok_or_else(|| fail(403, "verified applicant required"))?;
        return Ok((
            200,
            access::receipt(&access, &verified_owner, applicant, &id).await?,
        ));
    }
    if path == "/std/contact" && method == Method::PUT {
        let doc: Value = serde_json::from_slice(&bytes).map_err(|_| fail(400, "invalid JSON"))?;
        return Ok((
            200,
            directory::put(&access, &doc, service.as_ref(), now()).await?,
        ));
    }
    if path == "/std/contact"
        && method == Method::GET
        && service.is_some()
        && let Some(target) = parameter(&target, "name")?
        && !target.contains('#')
        && visitor
            .as_ref()
            .is_none_or(|person| person.name() != name(&target).unwrap_or_default())
    {
        let target = name(&target)?;
        let contact = access
            .find_contact_by_name(&target)
            .await
            .map_err(|error| fail(404, error.to_string()))?;
        let status = access_control::ContactStatus::try_from(contact.status)
            .map_err(|_| fail(500, "invalid contact status"))?;
        return Ok((
            200,
            json!({"name":contact.name,"status":status,"subject_id":String::from_utf8_lossy(contact.subject_id.as_bytes())}),
        ));
    }
    let directory_target = parameter(&target, "name")?.filter(|name| name.contains("~#"));
    if path == "/std/contact"
        && let Some(target) = directory_target
    {
        if service.is_some() || !owner {
            if method != Method::GET {
                return Err(fail(403, "verified owner required"));
            }
            let source = service
                .as_ref()
                .ok_or_else(|| fail(403, "verified owner required"))?;
            if !source.0.split_once('/').is_some_and(|(author, package)| {
                crate::package::is_standard_author(author) && package.starts_with("std/")
            }) {
                return Err(fail(403, "local standard service required"));
            }
        }
        let mut value = match method {
            Method::GET => directory::entry(access.database(), &target).await?,
            Method::PATCH => {
                let doc = serde_json::from_slice(&bytes).map_err(|_| fail(400, "invalid JSON"))?;
                directory::patch(&access, &target, &doc, now()).await?
            }
            Method::DELETE => return Err(fail(405, "use Group to leave a group")),
            _ => return Err(fail(405, "method not allowed")),
        };
        if service
            .as_ref()
            .is_some_and(|(id, _)| !owner || !directory::is_group_provider(id))
        {
            value.as_object_mut().unwrap().remove("alias");
        }
        return Ok((200, value));
    }
    if method == Method::PATCH {
        return native(
            method,
            target,
            Some(serde_json::from_slice(&bytes).map_err(|_| fail(400, "invalid JSON"))?),
        )
        .await;
    }
    let kind = parameter(&target, "kind")?.unwrap_or_else(|| "name".into());
    if !["name", "group", "all"].contains(&kind.as_str()) {
        return Err(fail(400, "invalid contact kind"));
    }
    if kind != "name" && (!owner || service.is_some()) {
        return Err(fail(403, "verified owner required"));
    }
    if path == "/std/contact/list" && method == Method::GET {
        // The incoming router has already consumed any one-shot permission for
        // this list. Rechecking it here would spend a second approval.
        let list = tokio::task::spawn_blocking(move || listing::read(&profile, &target, &kind))
            .await
            .map_err(|e| fail(500, e.to_string()))??;
        return Ok((200, list));
    }
    let manager = if method == Method::GET
        && let Some(visitor) = &visitor
    {
        matches!(
            access
                .auth(
                    access_control::Headers {
                        method: Method::GET,
                        path: "/std/contact/list".into(),
                        fields: http::HeaderMap::new()
                    },
                    Some(visitor.name()),
                    Some(visitor.subject_id())
                )
                .await
                .map_err(|error| fail(500, error.to_string()))?,
            access_control::AuthResult::Allowed
        )
    } else {
        false
    };
    if method == Method::GET {
        if !manager {
            if service.is_none()
                && let Some(person) = visitor.as_ref()
                && let Some(other) = parameter(&target, "name")?
            {
                let other = name(&other)?;
                let segment = crate::routes::identity_segment(&other, &["list", "requests"])
                    .map_err(|_| fail(400, "invalid contact name"))?;
                if person.name() != other
                    && access::shared_detail(&access, person, &segment).await?
                {
                    let mut detail = contact(&access, &endpoint, &other).await?;
                    detail["kind"] = "name".into();
                    if resource.is_some()
                        && detail["status"] == "active"
                        && let Some(subject) = detail["subject_id"].as_str()
                    {
                        detail["granted_access"] =
                            access::current_grants(access.database(), &other, subject.as_bytes())
                                .await?;
                    }
                    return Ok((200, detail));
                }
            }
            return native(method, target, None).await;
        }
        if path == "/std/contact" {
            let name =
                name(&parameter(&target, "name")?.ok_or_else(|| fail(400, "name is required"))?)?;
            let applicant_name = name.clone();
            let draft = tokio::task::spawn_blocking(move || {
                let db = contact_db
                    .lock()
                    .map_err(|_| fail(500, "contact database lock poisoned"))?;
                outbound_request(&db, &name)
            })
            .await
            .map_err(|e| fail(500, e.to_string()))??;
            let response = native(Method::GET, target, None)
                .await
                .map(|(status, mut value)| {
                    value["kind"] = "name".into();
                    (status, value)
                });
            let response = match response {
                Ok((status, mut value)) => {
                    if resource.is_some()
                        && value["status"] == "active"
                        && let Some(subject) = value["subject_id"].as_str()
                    {
                        value["granted_access"] = access::current_grants(
                            access.database(),
                            &applicant_name,
                            subject.as_bytes(),
                        )
                        .await?;
                    }
                    if let Some(subject) = value["subject_id"].as_str()
                        && let Some((id, received, deadline)) =
                            decisions::pending_application(&access, &applicant_name, subject)
                                .await?
                    {
                        value["incoming_application_id"] = id.clone().into();
                        value["incoming_received_at"] = received.into();
                        value["incoming_valid_before"] = deadline.into();
                        value["application_id"] = id.into();
                        capabilities::incoming_request(&mut value);
                    }
                    Ok((status, value))
                }
                error => error,
            };
            return match (response, draft.as_ref()) {
                (Ok((status, value)), Some(draft)) => Ok((status, project_contact(value, draft))),
                (Err((404, _)), Some(draft)) if draft["_contact_id"].is_null() => {
                    Ok((200, project_contact(Value::Null, draft)))
                }
                (response, _) => response,
            };
        }
        return Err(fail(404, "contact not found"));
    }
    if method == Method::DELETE {
        let batch = parameter(&target, "name")?.is_none();
        let names = if let Some(name_value) = parameter(&target, "name")? {
            if !bytes.is_empty() {
                return Err(fail(400, "provide name or names, not both"));
            }
            vec![name(&name_value)?]
        } else {
            let value: Value =
                serde_json::from_slice(&bytes).map_err(|_| fail(400, "names array is required"))?;
            if value.as_object().is_none_or(|object| object.len() != 1) {
                return Err(fail(400, "only names is accepted"));
            }
            value["names"]
                .as_array()
                .filter(|values| !values.is_empty())
                .ok_or_else(|| fail(400, "names must not be empty"))?
                .iter()
                .map(|value| {
                    name(
                        value
                            .as_str()
                            .ok_or_else(|| fail(400, "invalid contact name"))?,
                    )
                })
                .collect::<Result<Vec<_>, _>>()?
        };
        if batch {
            let person = visitor
                .as_ref()
                .ok_or_else(|| fail(403, "verified visitor required"))?;
            for name in &names {
                let segment = crate::routes::identity_segment(name, &["list", "requests"])
                    .map_err(|_| fail(400, "invalid name"))?;
                let headers = access_control::Headers {
                    method: Method::DELETE,
                    path: format!("/std/contact/{segment}"),
                    fields: Default::default(),
                };
                if !matches!(
                    access
                        .auth(headers, Some(person.name()), Some(person.subject_id()))
                        .await
                        .map_err(|e| fail(500, e.to_string()))?,
                    access_control::AuthResult::Allowed
                ) {
                    return Err(fail(403, "contact deletion denied by ACL"));
                }
            }
        }
        let db = database(&profile)?;
        db.execute_batch("BEGIN IMMEDIATE").map_err(storage)?;
        let mut persisted = Vec::new();
        for name in &names {
            let lease: Option<Option<i64>> = db
                .query_row(
                    "SELECT lease_until FROM outbound_contact_request WHERE target_name=?1",
                    [name],
                    |r| r.get(0),
                )
                .optional()
                .map_err(storage)?;
            if lease.flatten().is_some_and(|until| until > now()) {
                return Err(fail(409, "contact delivery is in progress; retry deletion"));
            }
            match access.find_contact_by_name(name).await {
                Ok(_) => persisted.push(name.clone()),
                Err(sea_orm::DbErr::RecordNotFound(_)) if lease.is_some() => {}
                Err(sea_orm::DbErr::RecordNotFound(_)) => {
                    return Err(fail(404, "contact not found"));
                }
                Err(error) => return Err(fail(500, error.to_string())),
            }
            db.execute(
                "DELETE FROM outbound_contact_request WHERE target_name=?1",
                [name],
            )
            .map_err(storage)?;
        }
        if !persisted.is_empty() {
            native(
                Method::DELETE,
                "/std/contact".into(),
                Some(json!({"names":persisted})),
            )
            .await?;
        }
        db.execute_batch("COMMIT").map_err(storage)?;
        return Ok((204, Value::Null));
    }
    if path == "/std/contact" && method == Method::POST {
        let doc: Value = serde_json::from_slice(&bytes).map_err(|_| fail(400, "invalid JSON"))?;
        let from = name(string(&doc, "from")?)?;
        let to = name(string(&doc, "to")?)?;
        if from == to {
            return Err(fail(400, "cannot contact yourself"));
        }
        if from != endpoint.name() {
            if to != endpoint.name() {
                return Err(fail(400, "contact recipient does not match this identity"));
            }
            let applicant = visitor
                .as_ref()
                .filter(|p| p.name() == from)
                .ok_or_else(|| fail(403, "verified applicant required"))?;
            let id = string(&doc, "application_id")?.to_owned();
            decisions::install_application_trigger(&access).await?;
            let (status, _) = native(Method::POST, "/std/contact".into(), Some(doc)).await?;
            return Ok((
                status,
                access::receipt(&access, &verified_owner, applicant, &id).await?,
            ));
        }
        if service.is_some() {
            return Err(fail(403, "direct owner required"));
        }
        let verified_owner = access::owner(&endpoint)?;
        let visitor = visitor
            .as_ref()
            .filter(|visitor| {
                visitor.name() == verified_owner.name()
                    && visitor.subject_id() == verified_owner.subject_id()
            })
            .ok_or_else(|| fail(403, "verified owner required"))?;
        if doc.as_object().is_none_or(|object| {
            object.keys().any(|key| {
                ![
                    "from",
                    "to",
                    "description",
                    "requested_capabilities",
                    "offered_capabilities",
                    "expires_after",
                    "valid_before",
                ]
                .contains(&key.as_str())
            })
        }) {
            return Err(fail(400, "invalid contact request fields"));
        }
        let target = to;
        if target == endpoint.name() {
            return Err(fail(400, "cannot contact yourself"));
        }
        let description = string(&doc, "description")?;
        if description.chars().count() > 80 || description.chars().any(char::is_control) {
            return Err(fail(400, "invalid description"));
        }
        if resource.is_some() {
            let db = database(&profile)?;
            let pending:Option<(String,String)>=db.query_row("SELECT application_id,state FROM outbound_contact_request
                WHERE target_name=? AND state IN ('queued','pending') AND min(local_deadline,coalesce(remote_deadline,local_deadline))>unixepoch()",
                [&target],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(storage)?;
            if let Some((id, state)) = pending {
                return Ok((202, json!({"application_id":id,"state":state})));
            }
        }
        let existing = match contact(&access, &endpoint, &target).await {
            Ok(contact) => Some(contact),
            Err((404, _)) => None,
            Err(error) => return Err(error),
        };
        if existing
            .as_ref()
            .is_some_and(|contact| contact["status"] == "blocked")
        {
            return Err(fail(409, "local contact is blocked"));
        }
        let expected = existing.as_ref().and_then(|row| row["subject_id"].as_str());
        let selection = doc["requested_capabilities"]
            .as_array()
            .ok_or_else(|| fail(400, "invalid requested capabilities"))?;
        let mut catalog_subject = expected.map(str::to_owned);
        let requested = if selection.is_empty() {
            json!({"groups":[],"access":{}})
        } else {
            match remote(
                &endpoint,
                &target,
                Method::GET,
                "/std/acl/permissions",
                None,
                expected,
            )
            .await
            {
                Ok((_, catalog, sid)) => {
                    catalog_subject = Some(sid);
                    let mut ops = capabilities::expand(&catalog, selection)?;
                    capabilities::bind_peer(&mut ops, endpoint.name())?;
                    json!({"groups":ops.iter().map(|c|&c["id"]).collect::<Vec<_>>(),"access":capabilities::requested(&ops)})
                }
                Err(e) if matches!(e.0, 502 | 503 | 504) && expected.is_some() => {
                    let db = database(&profile)?;
                    let cached:Option<String>=db.query_row("SELECT requested FROM outbound_contact_request WHERE target_name=?1 AND target_subject=?2",
                        params![target,expected],|r|r.get(0)).optional().map_err(storage)?;
                    let cached = cached
                        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                        .filter(|v| v["groups"] == doc["requested_capabilities"])
                        .ok_or_else(|| {
                            fail(503, "verified remote permission catalog unavailable")
                        })?;
                    cached
                }
                Err(e) => return Err(e),
            }
        };
        let valid_before = request_expiry(&doc)?;
        let mut offered_capabilities = selected(
            &profile,
            &endpoint,
            &doc["offered_capabilities"],
            endpoint.name(),
        )
        .await?;
        capabilities::bind_peer(&mut offered_capabilities, &target)?;
        let operations = if offered_capabilities.is_empty() {
            Vec::new()
        } else {
            vec![(Method::PATCH, "/std/acl/allow".to_owned())]
        };
        for (method, path) in operations {
            let headers = access_control::Headers {
                method,
                path,
                fields: http::HeaderMap::new(),
            };
            if !matches!(
                access
                    .auth(headers, Some(visitor.name()), Some(visitor.subject_id()))
                    .await
                    .map_err(|e| fail(500, e.to_string()))?,
                access_control::AuthResult::Allowed
            ) {
                return Err(fail(
                    403,
                    "contact creation or offered capability denied by ACL",
                ));
            }
        }
        let offered = json!({"groups":offered_capabilities.iter().map(|c|&c["id"]).collect::<Vec<_>>(),"access":capabilities::offered(&offered_capabilities)});
        let db = database(&profile)?;
        db.execute_batch("BEGIN IMMEDIATE").map_err(storage)?;
        expire_requests(&db)?;
        let pending:Option<String>=db.query_row("SELECT application_id FROM outbound_contact_request WHERE target_name=?1 AND state IN ('queued','pending')",
            [&target],|r|r.get(0)).optional().map_err(storage)?;
        if let Some(id) = pending {
            if resource.is_some() {
                let state: String = db
                    .query_row(
                        "SELECT state FROM outbound_contact_request WHERE application_id=?",
                        [&id],
                        |r| r.get(0),
                    )
                    .map_err(storage)?;
                return Ok((202, json!({"application_id":id,"state":state})));
            }
            return Ok((
                409,
                json!({"error":"contact request already pending","application_id":id}),
            ));
        }
        let current = match contact(&access, &endpoint, &target).await {
            Ok(c) => Some(c),
            Err((404, _)) => None,
            Err(e) => return Err(e),
        };
        if existing.as_ref().map(|c| (&c["id"], &c["subject_id"]))
            != current.as_ref().map(|c| (&c["id"], &c["subject_id"]))
        {
            return Err(fail(409, "contact changed during application"));
        }
        if let Some(c) = &current
            && !offered_capabilities.is_empty()
        {
            access::apply_offered(
                &access,
                &endpoint,
                &target,
                string(c, "subject_id")?,
                &offered["access"],
            )
            .await?;
        }
        let time = now();
        db.execute(
            "DELETE FROM outbound_contact_request WHERE target_name=?1",
            [&target],
        )
        .map_err(storage)?;
        db.execute("INSERT INTO outbound_contact_request(target_name,description,requested,offered,application_id,state,local_deadline,next_attempt_at,created_at,updated_at,contact_id,target_subject)
            VALUES(?1,?2,?3,?4,?5,'queued',?6,?7,?7,?7,?8,?9)",params![target,description,requested.to_string(),offered.to_string(),random_id()?,valid_before,time,
            current.as_ref().and_then(|c|c["id"].as_i64()),catalog_subject]).map_err(storage)?;
        let draft =
            outbound_request(&db, &target)?.ok_or_else(|| fail(500, "missing inserted request"))?;
        db.execute_batch("COMMIT").map_err(storage)?;
        if resource.is_some() {
            return Ok((
                202,
                json!({"application_id":draft["application_id"],"state":"queued"}),
            ));
        }
        return Ok((202, project_contact(current.unwrap_or(Value::Null), &draft)));
    }
    Err(fail(405, "method not allowed"))
}

#[cfg(test)]
#[path = "../tests/unit/contact/tests.rs"]
mod tests;

fn request_expiry(body: &Value) -> Result<i64, Failure> {
    let time = now();
    let valid_before = match (body.get("expires_after"), body.get("valid_before")) {
        (Some(_), Some(_)) => return Err(fail(400, "use expires_after or valid_before")),
        (Some(seconds), None) => {
            let seconds = seconds
                .as_i64()
                .filter(|seconds| *seconds > 0)
                .ok_or_else(|| fail(400, "invalid expires_after"))?;
            time.checked_add(seconds)
                .ok_or_else(|| fail(400, "expiry overflow"))?
        }
        (None, Some(time)) => time
            .as_i64()
            .ok_or_else(|| fail(400, "invalid valid_before"))?,
        (None, None) => time + 7 * 86400,
    };
    if valid_before <= time {
        return Err(fail(400, "valid_before must be in the future"));
    }
    Ok(valid_before)
}
