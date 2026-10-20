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
    routing::{get, post},
};
use dhttp_home::identity::IdentityProfile;
use http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tower::ServiceExt;

type Failure = (u16, String);

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
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
    db.query_row("SELECT id,target_name,description,requested_capabilities,offered_capabilities,status,expired_after,delivery_deadline,remote_expired_after,last_checked_at,error_message,created_at,updated_at FROM outbound_contact_requests WHERE id=?1",[id],|r|Ok(json!({"id":r.get::<_,i64>(0)?,"target_name":r.get::<_,String>(1)?,"description":r.get::<_,String>(2)?,"requested_capabilities":serde_json::from_str::<Value>(&r.get::<_,String>(3)?).unwrap_or(Value::Null),"offered_capabilities":serde_json::from_str::<Value>(&r.get::<_,String>(4)?).unwrap_or(Value::Null),"status":r.get::<_,String>(5)?,"valid_before":r.get::<_,i64>(6)?,"delivery_deadline":r.get::<_,i64>(7)?,"remote_valid_before":r.get::<_,Option<i64>>(8)?,"last_checked_at":r.get::<_,Option<i64>>(9)?,"error_message":r.get::<_,Option<String>>(10)?,"created_at":r.get::<_,i64>(11)?,"updated_at":r.get::<_,i64>(12)?}))).optional().map_err(storage)?.ok_or_else(||fail(404,"contact request not found"))
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
        tx.pragma_update(None, "user_version", 2).map_err(storage)?;
    } else if version == 1 {
        tx.execute_batch("ALTER TABLE outbound_contact_requests ADD COLUMN contact_id INTEGER; ALTER TABLE outbound_contact_requests ADD COLUMN contact_subject TEXT; DROP TABLE IF EXISTS saved_contacts;").map_err(storage)?;
        tx.pragma_update(None, "user_version", 2).map_err(storage)?;
    } else if version != 2 {
        return Err(fail(500, "unsupported contact database version"));
    }
    tx.prepare("SELECT id,target_name,description,requested_capabilities,offered_capabilities,application_id,status,expired_after,delivery_deadline,remote_expired_after,next_attempt_at,attempt_count,lease_until,last_checked_at,error_message,created_at,updated_at,contact_id,contact_subject FROM outbound_contact_requests LIMIT 0").map_err(storage)?;
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
    let visitor = owner(endpoint)?;
    let headers = access_control::Headers {
        method: request.method().clone(),
        path: path.to_owned(),
        fields: request.headers().clone(),
    };
    match access
        .auth(headers, Some(visitor.name()), Some(visitor.subject_id()))
        .await
        .map_err(|e| fail(500, e.to_string()))?
    {
        access_control::AuthResult::Allowed => {}
        access_control::AuthResult::Denied => {
            return Err(fail(403, "contact operation denied by ACL"));
        }
        access_control::AuthResult::Reviewing(_) | access_control::AuthResult::Waiting(_) => {
            return Err(fail(409, "contact operation awaits ACL approval"));
        }
    }
    request.extensions_mut().insert(visitor);
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
        &format!("/std/contact?name={}", encoded(target)),
        None,
    )
    .await
}
fn encoded(value: &str) -> String {
    form_urlencoded::byte_serialize(value.as_bytes()).collect()
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
        let mut request = endpoint
            .request(method, uri)
            .header("if-reviewed".parse().unwrap(), "break".parse().unwrap());
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
            let reviewing = status == 400
                && serde_json::from_slice::<Value>(&bytes)
                    .is_ok_and(|body| body["error"] == "need_review");
            return Err(fail(
                status,
                if reviewing {
                    "need_review"
                } else {
                    "remote contact operation rejected"
                },
            ));
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

// Called by an ACL-authorized submission, before enqueueing. Delivery and
// acceptance never replay grants or restore permissions the owner revoked.
async fn grant_offered(
    access: &Arc<AccessService>,
    endpoint: &dhttp::Endpoint,
    target: &str,
    subject: &str,
    requests: &[Value],
    capabilities: &[Value],
    valid_before: i64,
) -> Result<(), Failure> {
    let existing = match contact(access, endpoint, target).await {
        Ok(contact) => contact,
        Err((404, _)) => {
            local_api(
                access,
                endpoint,
                Method::PUT,
                &format!("/std/contact?name={}", encoded(target)),
                Some(&json!({"subject_id":subject,"class":"Human","description":"","status":"pending","valid_before":valid_before,"requested_access":requested(requests),"offers":offered(capabilities)})),
            )
            .await?;
            contact(access, endpoint, target).await?
        }
        Err(error) => return Err(error),
    };
    if existing["subject_id"] != subject
        || !matches!(existing["status"].as_str(), Some("pending" | "active"))
    {
        return Err(fail(409, "local contact blocks offered capabilities"));
    }
    for capability in capabilities {
        for operation in capability["endpoints"].as_array().unwrap() {
            local_api(
                access,
                endpoint,
                Method::PATCH,
                &format!("/std/acl/allow/{}", encoded(target)),
                Some(
                    &json!({"api":operation["path"],"method":operation["method"],"effect":"allow"}),
                ),
            )
            .await?;
        }
    }
    Ok(())
}

pub(crate) fn contact_router(
    profile: IdentityProfile,
    endpoint: dhttp::Endpoint,
    access: Arc<AccessService>,
) -> Router {
    let avatar_state = (endpoint.clone(), access.clone());
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
        .route("/std/contact", post(handler.clone()))
        .route(
            "/std/contact/requests",
            get(handler.clone()).delete(handler.clone()),
        )
        .route("/std/contact/requests/refresh", post(handler))
        .route(
            "/std/contact/{name}/profile/avatar",
            get(contact_avatar).with_state(avatar_state),
        )
}

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
    let bytes = axum::body::to_bytes(
        request.into_body(),
        if method == Method::POST && path == "/std/contact" {
            65536
        } else {
            0
        },
    )
    .await
    .map_err(|_| fail(400, "invalid request body"))?;
    let doc = if path == "/std/contact" && method == Method::POST {
        let doc: Value = serde_json::from_slice(&bytes).map_err(|_| fail(400, "invalid JSON"))?;
        if doc.as_object().is_none_or(|object| {
            object.keys().any(|key| {
                ![
                    "target_name",
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
        let target = name(string(&doc, "target_name")?)?;
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
                "/std/acl/capabilities",
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
                let existing:Option<i64>=tx.query_row("SELECT id FROM outbound_contact_requests WHERE target_name=?1 AND status IN ('queued','pending')",[string(&doc,"target_name")?],|r|r.get(0)).optional().map_err(storage)?;
                if existing.is_some() {return Err(fail(409,"contact request already pending"));}
                let time=now();
                tx.execute("INSERT INTO outbound_contact_requests(target_name,description,requested_capabilities,offered_capabilities,application_id,status,expired_after,delivery_deadline,next_attempt_at,created_at,updated_at,contact_id,contact_subject) VALUES(?1,?2,?3,?4,?5,'queued',?6,?6,?7,?7,?7,?8,?9)",params![string(&doc,"target_name")?,string(&doc,"description")?,doc["requested_capabilities"].to_string(),doc["offered_capabilities"].to_string(),random_id()?,doc["valid_before"].as_i64(),time,doc["contact_id"].as_i64(),doc["contact_subject"].as_str()]).map_err(storage)?;
                let id=tx.last_insert_rowid();tx.commit().map_err(storage)?;
                Ok((201,outbound_request(&db,id)?))
            }
            ("/std/contact/requests",Method::GET) => {
                expire_requests(&db)?;
                if let Some(id)=parameter(&target,"id")? {return Ok((200,outbound_request(&db,id.parse().map_err(|_|fail(400,"invalid id"))?)?));}
                let (p,size)=page(&target)?;
                let total:i64=db.query_row("SELECT count(*) FROM outbound_contact_requests",[],|r|r.get(0)).map_err(storage)?;
                let ids=db.prepare("SELECT id FROM outbound_contact_requests ORDER BY id DESC LIMIT ?1 OFFSET ?2").map_err(storage)?.query_map(params![size,(p-1).saturating_mul(size)],|r|r.get::<_,i64>(0)).map_err(storage)?.collect::<rusqlite::Result<Vec<_>>>().map_err(storage)?;
                let items=ids.into_iter().map(|id|outbound_request(&db,id)).collect::<Result<Vec<_>,_>>()?;
                Ok((200,json!({"items":items,"total":total,"page":p,"page_size":size})))
            }
            ("/std/contact/requests",Method::DELETE) => {
                let count=db.execute("DELETE FROM outbound_contact_requests WHERE id=?1 AND (lease_until IS NULL OR lease_until<=?2)",params![integer(&target,"id",0)?,now()]).map_err(storage)?;
                if count==0 {return Err(fail(409,"request not found or in progress"));}
                Ok((204,Value::Null))
            }
            ("/std/contact/requests/refresh",Method::POST) => {
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
        let job:Option<(i64,String,String,String,String,String,String,Option<i64>,Option<String>)>=tx.query_row("SELECT id,target_name,application_id,description,requested_capabilities,offered_capabilities,status,contact_id,contact_subject FROM outbound_contact_requests WHERE status IN ('queued','pending','active') AND next_attempt_at<=?1 AND (status='active' OR expired_after>?1) AND (lease_until IS NULL OR lease_until<=?1) ORDER BY next_attempt_at,id LIMIT 1",[now()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?))).optional().map_err(storage)?;
        let lease=now()+120;
        if let Some((id,..))=&job {tx.execute("UPDATE outbound_contact_requests SET lease_until=?1 WHERE id=?2",params![lease,id]).map_err(storage)?;}
        tx.commit().map_err(storage)?;Ok::<_,Failure>((job,lease))
    }).await.map_err(|e|fail(500,e.to_string()))??;
    let Some((
        id,
        target,
        application_id,
        description,
        requested_caps,
        offered_caps,
        status,
        contact_id,
        contact_subject,
    )) = job
    else {
        return Ok(());
    };
    let outcome=async {
        let requests:Value=serde_json::from_str(&requested_caps).map_err(|_|fail(500,"invalid stored capabilities"))?;
        let offers:Value=serde_json::from_str(&offered_caps).map_err(|_|fail(500,"invalid stored capabilities"))?;
        let selected_requests=selected(profile,endpoint,&requests,&target).await?;
        let selected_offers=selected(profile,endpoint,&offers,endpoint.name()).await?;
        let existing=match contact(access,endpoint,&target).await {Ok(c)=>Some(c),Err((404,_))=>None,Err(e)=>return Err(e)};
        if let Some(id) = contact_id {
            if existing.as_ref().is_none_or(|c| c["id"] != id || c["subject_id"].as_str() != contact_subject.as_deref()
                || matches!(c["status"].as_str(), Some("blocked" | "expired"))) {
                return Err(fail(410, "local contact was removed, replaced or blocked"));
            }
        }
        let expected=existing.as_ref().and_then(|c|c["subject_id"].as_str());
        let (method,path,body)=if status=="queued" {
            (Method::POST,"/std/contact/application".to_owned(),Some(json!({"application_id":application_id,"description":description,"class":"Human","requested_access":requested(&selected_requests),"offers":offered(&selected_offers)})))
        } else {(Method::GET,format!("/std/contact/status?application_id={application_id}"),None)};
        let (_,receipt,sid)=remote(endpoint,&target,method,&path,body.as_ref(),expected).await?;
        if receipt["application_id"]!=application_id || receipt["name"]!=target || receipt["subject_id"]!=sid {return Err(fail(502,"contact receipt identity does not match authenticated peer"));}
        if contact_id.is_none() && existing.is_none() && matches!(receipt["status"].as_str(), Some("pending" | "active")) {
            local_api(access,endpoint,Method::PUT,&format!("/std/contact?name={}",encoded(&target)),Some(&json!({"subject_id":sid,"class":"Human","description":"","status":"pending","requested_access":requested(&selected_requests),"offers":offered(&selected_offers)}))).await?;
        }
        let current = contact(access, endpoint, &target).await?;
        if let Some(id) = contact_id {
            if current["id"] != id || current["subject_id"].as_str() != contact_subject.as_deref() {
                return Err(fail(410, "local contact changed during delivery"));
            }
        }
        let job_profile = profile.clone();
        let local_id = current["id"].as_i64().ok_or_else(|| fail(502, "missing contact ID"))?;
        let local_subject = string(&current, "subject_id")?.to_owned();
        tokio::task::spawn_blocking(move || {
            database(&job_profile)?.execute("UPDATE outbound_contact_requests SET contact_id=?1,contact_subject=?2 WHERE id=?3 AND lease_until=?4 AND contact_id IS NULL",params![local_id,local_subject,id,lease]).map_err(storage)?;
            Ok::<_, Failure>(())
        }).await.map_err(|e| fail(500, e.to_string()))??;
        if receipt["status"]=="active" {
            let c=match contact(access,endpoint,&target).await {
                Ok(c)=>c,
                Err((404,_))=> return Err(fail(410,"local contact was removed")),
                Err(e)=>return Err(e),
            };
            if c["subject_id"]!=sid || matches!(c["status"].as_str(),Some("blocked"|"expired")) {return Err(fail(409,"local contact blocks reconciliation"));}
            if matches!(c["status"].as_str(),Some("pending"|"transfered")) {local_api(access,endpoint,Method::PATCH,&format!("/std/contact?name={}",encoded(&target)),Some(&json!({"status":"active"}))).await?;}
        }
        if receipt["status"]=="pending" && receipt["valid_before"].as_i64().is_none_or(|expires|expires<=now()) {return Err(fail(410,"remote contact request expired"));}
        let state=string(&receipt,"status")?;
        if !matches!(state,"pending"|"active"|"denied"|"expired"|"revoked") {return Err(fail(502,"unknown contact status"));}
        if receipt["valid_before"].as_i64().is_none() {return Err(fail(502,"missing contact expiry"));}
        Ok(receipt)
    }.await;
    let profile = profile.clone();
    tokio::task::spawn_blocking(move|| {
        let db=database(&profile)?;
        match outcome {
            Ok(receipt)=> {
                db.execute("UPDATE outbound_contact_requests SET status=?1,remote_expired_after=?2,expired_after=CASE WHEN ?1='pending' THEN min(delivery_deadline,?2) ELSE expired_after END,last_checked_at=?3,next_attempt_at=?4,attempt_count=attempt_count+1,error_message=NULL,lease_until=NULL,updated_at=?3 WHERE id=?5 AND lease_until=?6",params![string(&receipt,"status")?,receipt["valid_before"].as_i64().unwrap(),now(),now()+if receipt["status"]=="active" {30}else{3},id,lease]).map_err(storage)?;
            }
            Err((code,error))=> {
                db.execute("UPDATE outbound_contact_requests SET status=CASE WHEN ?1 THEN 'failed' ELSE status END,attempt_count=attempt_count+CASE WHEN ?2='need_review' THEN 0 ELSE 1 END,error_message=?2,next_attempt_at=?3,lease_until=NULL,last_checked_at=?4,updated_at=?4 WHERE id=?5 AND lease_until=?6",params![error != "need_review" && matches!(code,400|403|404|410),error,now()+if error == "need_review" {30} else {5},now(),id,lease]).map_err(storage)?;
            }
        }
        Ok(())
    }).await.map_err(|e|fail(500,e.to_string()))?
}

#[cfg(test)]
#[path = "../tests/unit/contact.rs"]
mod tests;
