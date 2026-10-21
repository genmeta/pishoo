//! Native contact collections and outbound delivery; daccess owns relationships and ACL.
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use access_control::AccessService;
use axum::{
    Router,
    body::Body,
    response::{IntoResponse, Response},
    routing::get,
};
use dhttp_home::identity::IdentityProfile;
use http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tower::ServiceExt;

mod access;
mod capabilities;
mod delivery;
mod storage;

pub(crate) use delivery::contact_jobs;
pub(crate) use storage::database;

use self::{
    access::{contact, encoded, grant_offered},
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
    if p < 1 || !(1..=100).contains(&size) {
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
        contact = json!({"id":null,"name":draft["target_name"],"subject_id":null,"alias":null,"class":"Human",
            "description":draft["description"],"status":"pending","incoming":false,
            "created_at":draft["created_at"],"updated_at":draft["updated_at"],"valid_before":draft["valid_before"],
            "requested_access":{},"granted_access":{},"offers":{}});
    }
    contact["updated_at"] = contact["updated_at"]
        .as_i64()
        .unwrap_or(0)
        .max(draft["updated_at"].as_i64().unwrap_or(0))
        .into();
    contact["delivery_status"] = draft["status"].clone();
    contact["delivery_error"] = draft["error_message"].clone();
    contact
}

pub(crate) fn contact_router(
    profile: IdentityProfile,
    endpoint: dhttp::Endpoint,
    access: Arc<AccessService>,
) -> Router {
    let avatar_state = (endpoint.clone(), access.clone());
    crate::routes::access_router(access.clone())
        .route(
            "/std/contact/{name}/profile/avatar",
            get(contact_avatar).with_state(avatar_state),
        )
        .layer(axum::middleware::from_fn(
            move |request: Request<Body>, next: axum::middleware::Next| {
                let (profile, endpoint, access) =
                    (profile.clone(), endpoint.clone(), access.clone());
                async move {
                    let path = request.uri().path();
                    if !((path == "/std/contact"
                        && matches!(
                            *request.method(),
                            Method::GET | Method::POST | Method::DELETE
                        ))
                        || path == "/std/contact/list" && *request.method() == Method::GET)
                    {
                        return next.run(request).await;
                    }
                    let result = dispatch(profile, endpoint, access, request).await;
                    let mut response = match result {
                        Ok((204, _)) => StatusCode::NO_CONTENT.into_response(),
                        Ok((status, value)) => {
                            (StatusCode::from_u16(status).unwrap(), axum::Json(value))
                                .into_response()
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
    profile: IdentityProfile,
    endpoint: dhttp::Endpoint,
    access: Arc<AccessService>,
    request: Request<Body>,
) -> Result<(u16, Value), Failure> {
    let visitor = request
        .extensions()
        .get::<access_control::Visitor>()
        .cloned();
    let method = request.method().clone();
    if matches!(method, Method::POST | Method::PUT | Method::DELETE)
        && request
            .headers()
            .get("sec-fetch-site")
            .is_some_and(|v| v == "cross-site")
    {
        return Err(fail(403, "cross-origin request denied"));
    }
    let target = request.uri().path_and_query().unwrap().as_str().to_owned();
    let path = request.uri().path().to_owned();
    let request_query = request.uri().query().unwrap_or("").to_owned();
    let bytes = axum::body::to_bytes(
        request.into_body(),
        if (method == Method::POST || method == Method::DELETE) && path == "/std/contact" {
            65536
        } else {
            0
        },
    )
    .await
    .map_err(|_| fail(400, "invalid request body"))?;
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
    let manager = if let Some(visitor) = &visitor {
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
            return native(method, target, None).await;
        }
        let request_profile = profile.clone();
        let drafts = tokio::task::spawn_blocking(move|| {
            let db = database(&request_profile)?;
            let mut statement = db.prepare("SELECT id FROM outbound_contact_request WHERE status<>'revoked' ORDER BY id DESC").map_err(storage)?;
            let ids = statement.query_map([],|row|row.get::<_,i64>(0)).map_err(storage)?.collect::<rusqlite::Result<Vec<_>>>().map_err(storage)?;
            let mut entries = std::collections::BTreeMap::new();
            for id in ids {
                let mut value = outbound_request(&db,id)?;
                let binding:(Option<i64>,Option<String>)=db.query_row("SELECT contact_id,contact_subject FROM outbound_contact_request WHERE id=?1",[id],|row|Ok((row.get(0)?,row.get(1)?))).map_err(storage)?;
                value["_contact_id"]=binding.0.into(); value["_contact_subject"]=binding.1.into();
                entries.entry(value["target_name"].as_str().unwrap().to_owned()).or_insert(value);
            }
            Ok::<_,Failure>(entries)
        }).await.map_err(|error|fail(500,error.to_string()))??;
        if path == "/std/contact" {
            let name =
                name(&parameter(&target, "name")?.ok_or_else(|| fail(400, "name is required"))?)?;
            let response = native(Method::GET, target, None).await;
            return match (response, drafts.get(&name)) {
                (Ok((status, value)), Some(draft)) => Ok((status, project_contact(value, draft))),
                (Err((404, _)), Some(draft))
                    if draft["_contact_id"].is_null()
                        && (draft["status"] == "queued" || draft["status"] == "failed") =>
                {
                    Ok((200, project_contact(Value::Null, draft)))
                }
                (response, _) => response,
            };
        }
        let (requested_page, size) = page(&target)?;
        let params = form_urlencoded::parse(request_query.as_bytes())
            .filter(|(key, _)| key != "page" && key != "page_size")
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect::<Vec<_>>();
        let mut items = Vec::new();
        let mut current = 1;
        let deleted = loop {
            let uri = {
                let mut query = form_urlencoded::Serializer::new(String::new());
                query
                    .extend_pairs(
                        params
                            .iter()
                            .map(|(key, value)| (key.as_str(), value.as_str())),
                    )
                    .append_pair("page", &current.to_string())
                    .append_pair("page_size", "100");
                format!("/std/contact/list?{}", query.finish())
            };
            let (_, value) = native(Method::GET, uri, None).await?;
            let entries = value["items"]
                .as_array()
                .ok_or_else(|| fail(500, "invalid contact list"))?;
            items.extend(entries.iter().cloned());
            if entries.is_empty() || current * 100 >= value["total"].as_i64().unwrap_or(0) {
                break value["deleted"].clone();
            }
            current += 1;
        };
        for (name, draft) in drafts {
            if let Some(contact) = items.iter_mut().find(|contact| contact["name"] == name) {
                *contact = project_contact(contact.clone(), &draft);
            } else if draft["_contact_id"].is_null()
                && (draft["status"] == "queued" || draft["status"] == "failed")
            {
                let value = project_contact(Value::Null, &draft);
                let search = parameter(&target, "search")?
                    .unwrap_or_default()
                    .to_lowercase();
                let since = integer(&target, "modified_since", 0)?;
                if name.to_lowercase().contains(&search)
                    && value["updated_at"].as_i64().unwrap_or(0) >= since
                {
                    items.push(value);
                }
            }
        }
        let key = parameter(&target, "sort")?.unwrap_or_else(|| "updated_at".into());
        let key = if key == "names" { "name" } else { key.as_str() };
        let asc = parameter(&target, "order")?.as_deref() == Some("asc");
        items.sort_by(|a, b| {
            let order = if matches!(key, "created_at" | "updated_at") {
                a[key].as_i64().cmp(&b[key].as_i64())
            } else {
                a[key]
                    .as_str()
                    .unwrap_or("")
                    .cmp(b[key].as_str().unwrap_or(""))
            };
            let order = order.then_with(|| a["name"].as_str().cmp(&b["name"].as_str()));
            if asc { order } else { order.reverse() }
        });
        let total = items.len();
        let items = items
            .into_iter()
            .skip(((requested_page - 1) * size) as usize)
            .take(size as usize)
            .collect::<Vec<_>>();
        return Ok((
            200,
            json!({"items":items,"total":total,"page":requested_page,"page_size":size,"deleted":deleted}),
        ));
    }
    if method == Method::DELETE {
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
        let mut persisted = Vec::new();
        for name in &names {
            match access.find_contact_by_name(name).await {
                Ok(_) => persisted.push(name.clone()),
                Err(sea_orm::DbErr::RecordNotFound(_)) => {
                    let db = database(&profile)?;
                    let exists:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM outbound_contact_request WHERE target_name=?1)",[name],|row|row.get(0)).map_err(storage)?;
                    if !exists {
                        return Err(fail(404, "contact not found"));
                    }
                }
                Err(error) => return Err(fail(500, error.to_string())),
            }
        }
        let request_profile = profile.clone();
        let targets = names.clone();
        let cancelled=tokio::task::spawn_blocking(move|| {
            let mut db=database(&request_profile)?;
            let tx=db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(storage)?;
            let mut previous=Vec::new();
            for name in &targets {
                let rows=tx.prepare("SELECT id,status,updated_at,lease_until FROM outbound_contact_request WHERE target_name=?1 AND status<>'revoked'").map_err(storage)?
                    .query_map([name],|row|Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?,row.get::<_,i64>(2)?,row.get::<_,Option<i64>>(3)?))).map_err(storage)?
                    .collect::<rusqlite::Result<Vec<_>>>().map_err(storage)?;
                if rows.iter().any(|row|row.3.is_some_and(|lease|lease>now())) {return Err(fail(409,"contact delivery is in progress; retry deletion"));}
                previous.extend(rows.into_iter().map(|(id,status,updated,_)|(id,status,updated)));
                tx.execute("UPDATE outbound_contact_request SET status='revoked',lease_until=NULL,updated_at=?1 WHERE target_name=?2",params![now(),name]).map_err(storage)?;
            }
            tx.commit().map_err(storage)?;
            Ok::<_,Failure>(previous)
        }).await.map_err(|error|fail(500,error.to_string()))??;
        let outcome = if persisted.is_empty() {
            Ok((204, Value::Null))
        } else {
            native(
                Method::DELETE,
                "/std/contact".into(),
                Some(json!({"names":persisted})),
            )
            .await
        };
        if outcome.is_err() {
            let mut db = database(&profile)?;
            let tx = db
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage)?;
            for (id, status, updated) in cancelled {
                tx.execute("UPDATE outbound_contact_request SET status=?1,updated_at=?2 WHERE id=?3 AND status='revoked'",params![status,updated,id]).map_err(storage)?;
            }
            tx.commit().map_err(storage)?;
        }
        return outcome;
    }
    let doc = if path == "/std/contact" && method == Method::POST {
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
            return native(Method::POST, "/std/contact".into(), Some(doc)).await;
        }
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
        let requested_capabilities =
            selected(&profile, &endpoint, &doc["requested_capabilities"], &target).await?;
        let valid_before = request_expiry(&doc)?;
        let offered_capabilities = selected(
            &profile,
            &endpoint,
            &doc["offered_capabilities"],
            endpoint.name(),
        )
        .await?;
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
        let visitor = visitor
            .as_ref()
            .ok_or_else(|| fail(403, "verified manager required"))?;
        let mut operations = vec![(Method::PUT, "/std/contact".to_owned())];
        if !offered_capabilities.is_empty() {
            operations.push((
                Method::PATCH,
                format!("/std/acl/allow/{}", encoded(&target)),
            ));
        }
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
        let pinned = if offered_capabilities.is_empty() {
            existing
        } else {
            let (_, _, subject) = remote(
                &endpoint,
                &target,
                Method::GET,
                "/std/acl/permissions",
                None,
                existing
                    .as_ref()
                    .and_then(|contact| contact["subject_id"].as_str()),
            )
            .await?;
            grant_offered(
                &access,
                &endpoint,
                &target,
                &subject,
                &requested_capabilities,
                &offered_capabilities,
                valid_before,
            )
            .await?;
            Some(contact(&access, &endpoint, &target).await?)
        };
        let mut doc = doc;
        doc["valid_before"] = valid_before.into();
        doc["contact_id"] = pinned
            .as_ref()
            .map_or(Value::Null, |contact| contact["id"].clone());
        doc["contact_subject"] = pinned
            .as_ref()
            .map_or(Value::Null, |contact| contact["subject_id"].clone());
        doc["_contact"] = pinned.unwrap_or(Value::Null);
        doc["target_name"] = target.into();
        doc
    } else {
        Value::Null
    };
    tokio::task::spawn_blocking(move|| {
        let mut db=database(&profile)?;
        match (path.as_str(),method) {
            ("/std/contact",Method::POST) => {
                expire_requests(&db)?;
                let tx=db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(storage)?;
                let existing:Option<i64>=tx.query_row("SELECT id FROM outbound_contact_request WHERE target_name=?1 AND status IN ('queued','pending')",[string(&doc,"target_name")?],|r|r.get(0)).optional().map_err(storage)?;
                if existing.is_some() {return Err(fail(409,"contact request already pending"));}
                let time=now();
                tx.execute("INSERT INTO outbound_contact_request(target_name,description,requested_capabilities,offered_capabilities,application_id,status,expired_after,delivery_deadline,next_attempt_at,created_at,updated_at,contact_id,contact_subject) VALUES(?1,?2,?3,?4,?5,'queued',?6,?6,?7,?7,?7,?8,?9)",params![string(&doc,"target_name")?,string(&doc,"description")?,doc["requested_capabilities"].to_string(),doc["offered_capabilities"].to_string(),random_id()?,doc["valid_before"].as_i64(),time,doc["contact_id"].as_i64(),doc["contact_subject"].as_str()]).map_err(storage)?;
                let id=tx.last_insert_rowid();tx.commit().map_err(storage)?;
                let mut draft=outbound_request(&db,id)?;
                draft["_contact_id"] = doc["contact_id"].clone();
                draft["_contact_subject"] = doc["contact_subject"].clone();
                Ok((202,project_contact(doc["_contact"].clone(),&draft)))
            }
            _ => Err(fail(405,"method not allowed")),
        }
    }).await.map_err(|e|fail(500,e.to_string()))?
}

#[cfg(test)]
#[path = "../tests/unit/contact.rs"]
mod tests;

async fn contact_avatar(
    axum::extract::State((endpoint, access)): axum::extract::State<(
        dhttp::Endpoint,
        Arc<AccessService>,
    )>,
    axum::extract::Path(raw_name): axum::extract::Path<String>,
) -> Response {
    let result = async {
        let target = name(&raw_name)?;
        let contact = access
            .find_contact_by_name(&target)
            .await
            .map_err(|error| {
                let status = if matches!(error, sea_orm::DbErr::RecordNotFound(_)) {
                    404
                } else {
                    500
                };
                fail(status, error.to_string())
            })?;
        let subject = std::str::from_utf8(contact.subject_id.as_bytes())
            .map_err(|_| fail(409, "invalid contact identity"))?;
        let expected = dhttp_home::certificate::OwnerHash::try_from(subject)
            .map_err(|_| fail(409, "invalid contact identity"))?;
        let uri = format!("https://{target}/std/profile/avatar")
            .parse::<http::Uri>()
            .map_err(|_| fail(400, "invalid avatar URI"))?;
        let (mut writer, response) = endpoint
            .request(Method::GET, uri)
            .expect_remote_owner_hash(expected)
            .body(dhttp::WndBuf::new(65536))
            .await
            .map_err(|error| fail(502, error.to_string()))?;
        writer
            .shutdown()
            .await
            .map_err(|error| fail(502, error.to_string()))?;
        let response = response
            .await
            .map_err(|error| fail(502, error.to_string()))?;
        let authority = response
            .extensions()
            .get::<dhttp::RemoteAuthority>()
            .ok_or_else(|| fail(502, "missing authenticated avatar identity"))?;
        if authority.name() != target {
            return Err(fail(502, "avatar authority mismatch"));
        }
        if response.status() != StatusCode::OK {
            return Err(fail(
                response.status().as_u16(),
                "contact avatar unavailable",
            ));
        }
        let content_type = response
            .headers()
            .get(http::header::CONTENT_TYPE)
            .cloned()
            .ok_or_else(|| fail(502, "missing avatar content type"))?;
        if !matches!(
            content_type.to_str().unwrap_or_default(),
            "image/png" | "image/jpeg" | "image/webp"
        ) {
            return Err(fail(502, "unsupported avatar content type"));
        }
        let mut body = response.into_body();
        let mut bytes = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|error| fail(502, error.to_string()))?;
            if let Ok(data) = frame.into_data() {
                if data.len() > 2 * 1024 * 1024 - bytes.len() {
                    return Err(fail(502, "avatar too large"));
                }
                bytes.extend_from_slice(&data);
            }
        }
        let mut response = bytes.into_response();
        response
            .headers_mut()
            .insert(http::header::CONTENT_TYPE, content_type);
        // The peer owns the image. Re-fetch it on the next request; there is no
        // local override or independent avatar state to reconcile.
        response.headers_mut().insert(
            http::header::CACHE_CONTROL,
            http::HeaderValue::from_static("no-store"),
        );
        Ok(response)
    };
    match tokio::time::timeout(Duration::from_secs(15), result).await {
        Ok(Ok(response)) => response,
        Ok(Err((status, message))) => (
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            message,
        )
            .into_response(),
        Err(_) => StatusCode::GATEWAY_TIMEOUT.into_response(),
    }
}

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
