//! Native contact collections and outbound delivery; daccess owns relationships and ACL.
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use access_control::AccessService;
use axum::{Router, body::Body, response::IntoResponse, routing::any};
use dhttp_home::identity::IdentityProfile;
use http::{Method, Request, StatusCode};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};

mod access;
mod capabilities;
mod delivery;
mod storage;

pub(crate) use delivery::contact_jobs;
pub(crate) use storage::database;

use self::{
    access::{contact, contacts, grant_offered, owner},
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
fn required(target: &str, key: &str) -> Result<String, Failure> {
    parameter(target, key)?.ok_or_else(|| fail(400, format!("missing {key}")))
}
fn target_name(target: &str) -> Result<String, Failure> {
    name(&required(target, "name")?)
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

pub(crate) fn contact_router(
    profile: IdentityProfile,
    endpoint: dhttp::Endpoint,
    access: Arc<AccessService>,
) -> Router {
    let handler = move |request: Request<Body>| {
        let (profile, endpoint, access) = (profile.clone(), endpoint.clone(), access.clone());
        async move {
            let result = dispatch(profile, endpoint, access, request).await;
            let mut response = match result {
                Ok((204, _)) => StatusCode::NO_CONTENT.into_response(),
                Ok((status, value)) => {
                    (StatusCode::from_u16(status).unwrap(), axum::Json(value)).into_response()
                }
                Err((status, message)) => (
                    StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                    axum::Json(json!({"error":message})),
                )
                    .into_response(),
            };
            response
                .headers_mut()
                .insert(http::header::CACHE_CONTROL, "no-store".parse().unwrap());
            response
        }
    };
    Router::new()
        .route("/std/contact/local/saved", any(handler.clone()))
        .route("/std/contact/local/requests", any(handler.clone()))
        .route("/std/contact/local/requests/refresh", any(handler))
}
async fn dispatch(
    profile: IdentityProfile,
    endpoint: dhttp::Endpoint,
    access: Arc<AccessService>,
    request: Request<Body>,
) -> Result<(u16, Value), Failure> {
    let expected = owner(&endpoint)?;
    if request
        .extensions()
        .get::<access_control::Visitor>()
        .is_none_or(|visitor| {
            visitor.name() != expected.name() || visitor.subject_id() != expected.subject_id()
        })
    {
        return Err(fail(403, "owner required"));
    }
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
    let bytes = axum::body::to_bytes(
        request.into_body(),
        if method == Method::POST && path.ends_with("/requests") {
            65536
        } else {
            0
        },
    )
    .await
    .map_err(|_| fail(400, "invalid request body"))?;
    if path == "/std/contact/local/saved" {
        return match method {
            Method::GET => {
                let contacts = contacts(&access, &endpoint).await?;
                tokio::task::spawn_blocking(move|| {
                    let db=database(&profile)?; let mut saved=Vec::new();
                    for c in contacts {
                        let stored:Option<(i64,Vec<u8>)>=db.query_row("SELECT contact_id,subject_id FROM saved_contacts WHERE contact_name=?1",[string(&c,"name")?],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(storage)?;
                        if stored.is_some_and(|(id,sid)|c["id"]==id && c["subject_id"].as_str().is_some_and(|actual|sid==actual.as_bytes())) {saved.push(c["name"].clone());}
                    }
                    Ok((200,Value::Array(saved)))
                }).await.map_err(|e|fail(500,e.to_string()))?
            }
            Method::PUT => {
                let target = target_name(&target)?;
                let c = contact(&access, &endpoint, &target).await?;
                tokio::task::spawn_blocking(move|| {
                    let db=database(&profile)?;
                    db.execute("INSERT INTO saved_contacts(contact_name,contact_id,subject_id,saved_at) VALUES(?1,?2,?3,?4) ON CONFLICT(contact_name) DO UPDATE SET contact_id=excluded.contact_id,subject_id=excluded.subject_id,saved_at=excluded.saved_at",params![target,c["id"].as_i64().ok_or_else(||fail(502,"missing contact ID"))?,string(&c,"subject_id")?.as_bytes(),now()]).map_err(storage)?;
                    Ok((204,Value::Null))
                }).await.map_err(|e|fail(500,e.to_string()))?
            }
            Method::DELETE => {
                let target = target_name(&target)?;
                tokio::task::spawn_blocking(move || {
                    database(&profile)?
                        .execute("DELETE FROM saved_contacts WHERE contact_name=?1", [target])
                        .map_err(storage)?;
                    Ok((204, Value::Null))
                })
                .await
                .map_err(|e| fail(500, e.to_string()))?
            }
            _ => Err(fail(405, "method not allowed")),
        };
    }
    let doc = if path == "/std/contact/local/requests" && method == Method::POST {
        let doc: Value = serde_json::from_slice(&bytes).map_err(|_| fail(400, "invalid JSON"))?;
        if doc.as_object().is_none_or(|object| {
            object.keys().any(|key| {
                ![
                    "target_name",
                    "description",
                    "requested_capabilities",
                    "offered_capabilities",
                ]
                .contains(&key.as_str())
            })
        }) {
            return Err(fail(400, "invalid contact request fields"));
        }
        let target = name(string(&doc, "target_name")?)?;
        if target == endpoint.name() {
            return Err(fail(400, "cannot contact yourself"));
        }
        let description = string(&doc, "description")?;
        if description.chars().count() > 80 || description.chars().any(char::is_control) {
            return Err(fail(400, "invalid description"));
        }
        selected(&profile, &endpoint, &doc["requested_capabilities"], &target).await?;
        let offered_capabilities = selected(
            &profile,
            &endpoint,
            &doc["offered_capabilities"],
            endpoint.name(),
        )
        .await?;
        if !offered_capabilities.is_empty() {
            let existing = match contact(&access, &endpoint, &target).await {
                Ok(contact) => Some(contact),
                Err((404, _)) => None,
                Err(error) => return Err(error),
            };
            let (_, _, subject) = remote(
                &endpoint,
                &target,
                Method::GET,
                "/std/acl/promissions",
                None,
                existing
                    .as_ref()
                    .and_then(|contact| contact["subject_id"].as_str()),
            )
            .await?;
            grant_offered(&access, &endpoint, &target, &subject, &offered_capabilities).await?;
        }
        let mut doc = doc;
        doc["target_name"] = target.into();
        doc
    } else {
        Value::Null
    };
    tokio::task::spawn_blocking(move|| {
        let mut db=database(&profile)?;
        match (path.as_str(),method) {
            ("/std/contact/local/requests",Method::POST) => {
                expire_requests(&db)?;
                let tx=db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(storage)?;
                let existing:Option<i64>=tx.query_row("SELECT id FROM outbound_contact_requests WHERE target_name=?1 AND status IN ('queued','pending')",[string(&doc,"target_name")?],|r|r.get(0)).optional().map_err(storage)?;
                if existing.is_some() {return Err(fail(409,"contact request already pending"));}
                let time=now();
                tx.execute("INSERT INTO outbound_contact_requests(target_name,description,requested_capabilities,offered_capabilities,application_id,status,expired_after,delivery_deadline,next_attempt_at,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,'queued',?6,?6,?7,?7,?7)",params![string(&doc,"target_name")?,string(&doc,"description")?,doc["requested_capabilities"].to_string(),doc["offered_capabilities"].to_string(),random_id()?,time+7*86400,time]).map_err(storage)?;
                let id=tx.last_insert_rowid();tx.commit().map_err(storage)?;
                Ok((201,outbound_request(&db,id)?))
            }
            ("/std/contact/local/requests",Method::GET) => {
                expire_requests(&db)?;
                if let Some(id)=parameter(&target,"id")? {return Ok((200,outbound_request(&db,id.parse().map_err(|_|fail(400,"invalid id"))?)?));}
                let (p,size)=page(&target)?;
                let total:i64=db.query_row("SELECT count(*) FROM outbound_contact_requests",[],|r|r.get(0)).map_err(storage)?;
                let ids=db.prepare("SELECT id FROM outbound_contact_requests ORDER BY id DESC LIMIT ?1 OFFSET ?2").map_err(storage)?.query_map(params![size,(p-1).saturating_mul(size)],|r|r.get::<_,i64>(0)).map_err(storage)?.collect::<rusqlite::Result<Vec<_>>>().map_err(storage)?;
                let items=ids.into_iter().map(|id|outbound_request(&db,id)).collect::<Result<Vec<_>,_>>()?;
                Ok((200,json!({"items":items,"total":total,"page":p,"page_size":size})))
            }
            ("/std/contact/local/requests",Method::DELETE) => {
                let count=db.execute("DELETE FROM outbound_contact_requests WHERE id=?1 AND (lease_until IS NULL OR lease_until<=?2)",params![integer(&target,"id",0)?,now()]).map_err(storage)?;
                if count==0 {return Err(fail(409,"request not found or in progress"));}
                Ok((204,Value::Null))
            }
            ("/std/contact/local/requests/refresh",Method::POST) => {
                let id=integer(&target,"id",0)?;
                db.execute("UPDATE outbound_contact_requests SET next_attempt_at=?1 WHERE id=?2 AND status IN ('queued','pending','active')",params![now(),id]).map_err(storage)?;
                Ok((200,outbound_request(&db,id)?))
            }
            _ => Err(fail(405,"method not allowed")),
        }
    }).await.map_err(|e|fail(500,e.to_string()))?
}

#[cfg(test)]
#[path = "../tests/unit/contact.rs"]
mod tests;
