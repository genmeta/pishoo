//! Native contact collections and outbound delivery; daccess owns relationships and ACL.
use access_control::AccessService;
use axum::{Router, body::Body, response::IntoResponse, routing::any};
use dhttp_home::identity::IdentityProfile;
use http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::io::AsyncWriteExt;
use tower::ServiceExt;

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
fn storage(error: rusqlite::Error) -> Failure {
    let status = if matches!(&error, rusqlite::Error::SqliteFailure(code, _) if matches!(code.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
    {
        503
    } else {
        500
    };
    fail(status, format!("contact storage: {error}"))
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
fn requested(selected: &[Value]) -> Value {
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
fn offered(selected: &[Value]) -> Value {
    let mut access = requested(selected);
    for methods in access.as_object_mut().unwrap().values_mut() {
        *methods = json!({"allow":methods.clone(),"review":[],"deny":[]});
    }
    access
}

fn outbound_request(db: &Connection, id: i64) -> Result<Value, Failure> {
    db.query_row("SELECT id,target_name,description,requested_capabilities,offered_capabilities,status,expired_after,delivery_deadline,remote_expired_after,last_checked_at,error_message,created_at,updated_at FROM outbound_contact_requests WHERE id=?1",[id],|r|Ok(json!({"id":r.get::<_,i64>(0)?,"target_name":r.get::<_,String>(1)?,"description":r.get::<_,String>(2)?,"requested_capabilities":serde_json::from_str::<Value>(&r.get::<_,String>(3)?).unwrap_or(Value::Null),"offered_capabilities":serde_json::from_str::<Value>(&r.get::<_,String>(4)?).unwrap_or(Value::Null),"status":r.get::<_,String>(5)?,"expired_after":r.get::<_,i64>(6)?,"delivery_deadline":r.get::<_,i64>(7)?,"remote_expired_after":r.get::<_,Option<i64>>(8)?,"last_checked_at":r.get::<_,Option<i64>>(9)?,"error_message":r.get::<_,Option<String>>(10)?,"created_at":r.get::<_,i64>(11)?,"updated_at":r.get::<_,i64>(12)?}))).optional().map_err(storage)?.ok_or_else(||fail(404,"contact request not found"))
}
fn expire_requests(db: &Connection) -> Result<(), Failure> {
    db.execute("UPDATE outbound_contact_requests SET status=CASE WHEN status='queued' THEN 'failed' ELSE 'expired' END,lease_until=NULL,updated_at=?1,error_message='contact request expired' WHERE status IN ('queued','pending') AND expired_after<=?1",[now()]).map_err(storage)?;
    Ok(())
}

fn database(profile: &IdentityProfile) -> Result<Connection, Failure> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let path = profile.join("db/contact.db");
    match path.symlink_metadata() {
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(fail(500, "contact database must be a regular file"));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(fail(500, e.to_string())),
            }
        }
        Err(e) => return Err(fail(500, e.to_string())),
        _ => {}
    }
    if !path
        .symlink_metadata()
        .map_err(|e| fail(500, e.to_string()))?
        .file_type()
        .is_file()
    {
        return Err(fail(500, "contact database must be a regular file"));
    }
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| fail(500, e.to_string()))?;
    let mut db = Connection::open(&path).map_err(storage)?;
    db.busy_timeout(Duration::from_secs(2)).map_err(storage)?;
    db.pragma_update(None, "synchronous", "FULL")
        .map_err(storage)?;
    let tx = db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(storage)?;
    let version: i64 = tx
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(storage)?;
    if version == 0 {
        let count: i64 = tx
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
                [],
                |r| r.get(0),
            )
            .map_err(storage)?;
        if count != 0 {
            return Err(fail(500, "unrecognized contact database"));
        }
        tx.execute_batch(include_str!("contact.sql"))
            .map_err(storage)?;
        tx.pragma_update(None, "user_version", 1).map_err(storage)?;
    } else if version != 1 {
        return Err(fail(500, "unsupported contact database version"));
    }
    tx.prepare("SELECT contact_name,contact_id,subject_id,saved_at FROM saved_contacts LIMIT 0")
        .map_err(storage)?;
    tx.prepare("SELECT id,target_name,description,requested_capabilities,offered_capabilities,application_id,status,expired_after,delivery_deadline,remote_expired_after,next_attempt_at,attempt_count,lease_until,last_checked_at,error_message,created_at,updated_at FROM outbound_contact_requests LIMIT 0").map_err(storage)?;
    tx.commit().map_err(storage)?;
    Ok(db)
}
fn owner(endpoint: &dhttp::Endpoint) -> Result<access_control::Visitor, Failure> {
    let local = endpoint
        .local_authority()
        .map_err(|e| fail(500, e.to_string()))?;
    let ski = dhttp_home::certificate::extract_dhttp_subject_key_identifier(local.certificates())
        .map_err(|e| fail(500, e.to_string()))?;
    Ok(access_control::Visitor::new(
        endpoint.name(),
        access_control::SubjectId::new(ski.owner_hash().as_str().as_bytes())
            .map_err(|_| fail(500, "invalid local subject identity"))?,
    ))
}
async fn local_api(
    access: &Arc<AccessService>,
    endpoint: &dhttp::Endpoint,
    method: Method,
    path: &str,
    body: Option<&Value>,
) -> Result<Value, Failure> {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(
            body.map_or_else(Vec::new, |v| serde_json::to_vec(v).unwrap()),
        ))
        .map_err(|e| fail(500, e.to_string()))?;
    request.extensions_mut().insert(owner(endpoint)?);
    let response = crate::routes::access_router(access.clone())
        .oneshot(request)
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .map_err(|e| fail(502, e.to_string()))?;
    if !(200..300).contains(&status) {
        return Err(fail(status, String::from_utf8_lossy(&bytes).into_owned()));
    }
    if bytes.is_empty() {
        Ok(Value::Null)
    } else {
        serde_json::from_slice(&bytes).map_err(|_| fail(502, "invalid contact response"))
    }
}
async fn contact(
    access: &Arc<AccessService>,
    endpoint: &dhttp::Endpoint,
    target: &str,
) -> Result<Value, Failure> {
    local_api(
        access,
        endpoint,
        Method::GET,
        &format!("/std/contact/{}", encoded(target)),
        None,
    )
    .await
}
fn encoded(value: &str) -> String {
    form_urlencoded::byte_serialize(value.as_bytes()).collect()
}
async fn contacts(
    access: &Arc<AccessService>,
    endpoint: &dhttp::Endpoint,
) -> Result<Vec<Value>, Failure> {
    let mut items = Vec::new();
    let mut p = 1;
    loop {
        let value = local_api(
            access,
            endpoint,
            Method::GET,
            &format!("/std/contacts?page={p}&page_size=100&sort=name&order=asc"),
            None,
        )
        .await?;
        let chunk = value["items"]
            .as_array()
            .ok_or_else(|| fail(502, "invalid contact list"))?;
        items.extend(chunk.clone());
        if items.len() >= value["total"].as_u64().unwrap_or(0) as usize || chunk.is_empty() {
            return Ok(items);
        }
        p += 1;
    }
}
async fn remote(
    endpoint: &dhttp::Endpoint,
    target: &str,
    method: Method,
    path: &str,
    body: Option<&Value>,
    expected: Option<&str>,
) -> Result<(u16, Value, String), Failure> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let uri: http::Uri = format!("https://{target}{path}")
            .parse()
            .map_err(|_| fail(400, "invalid contact URI"))?;
        let mut request = endpoint.request(method, uri);
        if let Some(expected) = expected {
            let hash = dhttp_home::certificate::OwnerHash::try_from(expected)
                .map_err(|_| fail(409, "invalid contact identity"))?;
            request = request.expect_remote_owner_hash(hash);
        }
        let response = if let Some(body) = body {
            let (mut writer, response) = request
                .header(
                    http::header::CONTENT_TYPE,
                    "application/json".parse().unwrap(),
                )
                .body(dhttp::WndBuf::new(65536))
                .await
                .map_err(|e| fail(502, e.to_string()))?;
            writer
                .write_all(&serde_json::to_vec(body).unwrap())
                .await
                .map_err(|e| fail(502, e.to_string()))?;
            writer
                .shutdown()
                .await
                .map_err(|e| fail(502, e.to_string()))?;
            response.await.map_err(|e| fail(502, e.to_string()))?
        } else {
            let (mut writer, response) = request
                .body(dhttp::WndBuf::new(65536))
                .await
                .map_err(|e| fail(502, e.to_string()))?;
            writer
                .shutdown()
                .await
                .map_err(|e| fail(502, e.to_string()))?;
            response.await.map_err(|e| fail(502, e.to_string()))?
        };
        let authority = response
            .extensions()
            .get::<dhttp::RemoteAuthority>()
            .ok_or_else(|| fail(502, "missing authenticated contact identity"))?;
        if authority.name() != target {
            return Err(fail(502, "contact authority mismatch"));
        }
        let sid =
            dhttp_home::certificate::extract_dhttp_subject_key_identifier(authority.certificates())
                .map_err(|_| fail(502, "invalid contact identity"))?
                .owner_hash()
                .as_str()
                .to_owned();
        let status = response.status().as_u16();
        let mut body = response.into_body();
        let mut bytes = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|e| fail(502, e.to_string()))?;
            let data = frame
                .into_data()
                .map_err(|_| fail(502, "contact response trailers are unsupported"))?;
            if data.len() > 1024 * 1024 - bytes.len() {
                return Err(fail(502, "contact response too large"));
            }
            bytes.extend_from_slice(&data);
        }
        if status != 200 && status != 201 {
            return Err(fail(status, "remote contact operation rejected"));
        }
        let value = serde_json::from_slice(&bytes).map_err(|_| fail(502, "invalid remote JSON"))?;
        Ok((status, value, sid))
    })
    .await
    .map_err(|_| fail(504, "contact request timed out"))?
}
fn catalog(profile: &IdentityProfile) -> Result<Value, Failure> {
    let db = Connection::open_with_flags(
        profile.config_db_path(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(storage)?;
    let mut items = Vec::new();
    let mut stmt=db.prepare("SELECT c.lib_id,c.capability_id,c.title,l.version FROM lib_capabilities c JOIN libs l ON l.id=c.lib_id ORDER BY c.lib_id,c.capability_id").map_err(storage)?;
    for row in stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })
        .map_err(storage)?
    {
        let (lib, id, title, version) = row.map_err(storage)?;
        let endpoints=db.prepare("SELECT method,path FROM lib_capability_operations WHERE lib_id=?1 AND capability_id=?2 ORDER BY method,path").map_err(storage)?
            .query_map(params![lib,id],|r|Ok(json!({"method":r.get::<_,String>(0)?,"path":r.get::<_,String>(1)?}))).map_err(storage)?.collect::<rusqlite::Result<Vec<_>>>().map_err(storage)?;
        items.push(json!({"id":format!("{lib}#{id}"),"title":title,"version":version,"selectable":true,"endpoints":endpoints}));
    }
    Ok(items.into())
}
async fn selected(
    profile: &IdentityProfile,
    endpoint: &dhttp::Endpoint,
    ids: &Value,
    target: &str,
) -> Result<Vec<Value>, Failure> {
    let ids = ids
        .as_array()
        .ok_or_else(|| fail(400, "invalid capabilities"))?;
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
            "/std/acl/capabilities",
            None,
            None,
        )
        .await?
        .1
    };
    let catalog = catalog
        .as_array()
        .ok_or_else(|| fail(502, "invalid capability catalog"))?;
    let mut result = Vec::new();
    for id in ids {
        let id = id
            .as_str()
            .ok_or_else(|| fail(400, "invalid capability ID"))?;
        if result.iter().any(|v: &Value| v["id"] == id) {
            return Err(fail(400, "duplicate capability"));
        }
        result.push(
            catalog
                .iter()
                .find(|v| v["id"] == id && v["selectable"] == true)
                .ok_or_else(|| fail(409, "selected capability unavailable"))?
                .clone(),
        );
    }
    Ok(result)
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
        selected(
            &profile,
            &endpoint,
            &doc["offered_capabilities"],
            endpoint.name(),
        )
        .await?;
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

pub(crate) async fn contact_jobs(
    profile: &IdentityProfile,
    endpoint: &dhttp::Endpoint,
    access: &Arc<AccessService>,
) -> crate::Result<()> {
    let result = deliver(profile, endpoint, access).await;
    result.map_err(|(_, message)| crate::Error::InvalidConfig(message))
}
async fn deliver(
    profile: &IdentityProfile,
    endpoint: &dhttp::Endpoint,
    access: &Arc<AccessService>,
) -> Result<(), Failure> {
    let job_profile = profile.clone();
    let (job,lease)=tokio::task::spawn_blocking(move|| {
        let mut db=database(&job_profile)?;expire_requests(&db)?;
        let tx=db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(storage)?;
        let job:Option<(i64,String,String,String,String,String,String)>=tx.query_row("SELECT id,target_name,application_id,description,requested_capabilities,offered_capabilities,status FROM outbound_contact_requests WHERE status IN ('queued','pending','active') AND next_attempt_at<=?1 AND (status='active' OR expired_after>?1) AND (lease_until IS NULL OR lease_until<=?1) ORDER BY next_attempt_at,id LIMIT 1",[now()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?))).optional().map_err(storage)?;
        let lease=now()+120;
        if let Some((id,..))=&job {tx.execute("UPDATE outbound_contact_requests SET lease_until=?1 WHERE id=?2",params![lease,id]).map_err(storage)?;}
        tx.commit().map_err(storage)?;Ok::<_,Failure>((job,lease))
    }).await.map_err(|e|fail(500,e.to_string()))??;
    let Some((id, target, application_id, description, requested_caps, offered_caps, status)) = job
    else {
        return Ok(());
    };
    let outcome=async {
        let requests:Value=serde_json::from_str(&requested_caps).map_err(|_|fail(500,"invalid stored capabilities"))?;
        let offers:Value=serde_json::from_str(&offered_caps).map_err(|_|fail(500,"invalid stored capabilities"))?;
        let selected_requests=selected(profile,endpoint,&requests,&target).await?;
        let selected_offers=selected(profile,endpoint,&offers,endpoint.name()).await?;
        let existing=match contact(access,endpoint,&target).await {Ok(c)=>Some(c),Err((404,_))=>None,Err(e)=>return Err(e)};
        let expected=existing.as_ref().and_then(|c|c["subject_id"].as_str());
        let (method,path,body)=if status=="queued" {
            (Method::POST,"/std/contact".to_owned(),Some(json!({"application_id":application_id,"description":description,"class":"Human","requested_access":requested(&selected_requests),"offers":offered(&selected_offers)})))
        } else {(Method::GET,format!("/std/contact/self?application_id={application_id}"),None)};
        let (_,receipt,sid)=remote(endpoint,&target,method,&path,body.as_ref(),expected).await?;
        if receipt["application_id"]!=application_id || receipt["name"]!=target || receipt["subject_id"]!=sid {return Err(fail(502,"contact receipt identity does not match authenticated peer"));}
        if receipt["status"]=="active" {
            let c=match contact(access,endpoint,&target).await {
                Ok(c)=>c,
                Err((404,_))=> {
                    local_api(access,endpoint,Method::PUT,&format!("/std/contact/{}",encoded(&target)),Some(&json!({"subject_id":sid,"class":"Human","description":"","status":"active","requested_access":requested(&selected_requests),"offers":offered(&selected_offers)}))).await?;
                    contact(access,endpoint,&target).await?
                }
                Err(e)=>return Err(e),
            };
            if c["subject_id"]!=sid || matches!(c["status"].as_str(),Some("blocked"|"expired")) {return Err(fail(409,"local contact blocks reconciliation"));}
            if matches!(c["status"].as_str(),Some("pending"|"transfered")) {local_api(access,endpoint,Method::PATCH,&format!("/std/contact/{}",encoded(&target)),Some(&json!({"status":"active"}))).await?;}
        }
        if receipt["status"]=="pending" && receipt["expired_after"].as_i64().is_none_or(|expires|expires<=now()) {return Err(fail(410,"remote contact request expired"));}
        let state=string(&receipt,"status")?;
        if !matches!(state,"pending"|"active"|"denied"|"expired"|"revoked") {return Err(fail(502,"unknown contact status"));}
        if receipt["expired_after"].as_i64().is_none() {return Err(fail(502,"missing contact expiry"));}
        Ok(receipt)
    }.await;
    let profile = profile.clone();
    tokio::task::spawn_blocking(move|| {
        let db=database(&profile)?;
        match outcome {
            Ok(receipt)=> {
                db.execute("UPDATE outbound_contact_requests SET status=?1,remote_expired_after=?2,expired_after=CASE WHEN ?1='pending' THEN min(delivery_deadline,?2) ELSE expired_after END,last_checked_at=?3,next_attempt_at=?4,attempt_count=attempt_count+1,error_message=NULL,lease_until=NULL,updated_at=?3 WHERE id=?5 AND lease_until=?6",params![string(&receipt,"status")?,receipt["expired_after"].as_i64().unwrap(),now(),now()+if receipt["status"]=="active" {30}else{3},id,lease]).map_err(storage)?;
            }
            Err((code,error))=> {
                db.execute("UPDATE outbound_contact_requests SET status=CASE WHEN ?1 THEN 'failed' ELSE status END,attempt_count=attempt_count+1,error_message=?2,next_attempt_at=?3,lease_until=NULL,last_checked_at=?4,updated_at=?4 WHERE id=?5 AND lease_until=?6",params![matches!(code,400|403|404|410),error,now()+5,now(),id,lease]).map_err(storage)?;
            }
        }
        Ok(())
    }).await.map_err(|e|fail(500,e.to_string()))?
}

#[cfg(test)]
#[path = "../tests/unit/contact.rs"]
mod tests;
