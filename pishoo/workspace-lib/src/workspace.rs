//! Ordinary user Lib: business data in /db, native resources accessed via HTTP.
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use wasip2::http::{outgoing_handler, types::*};

type Failure = (u16, String);
type Reply = (u16, String, Vec<u8>, Vec<(String, Vec<u8>)>);
struct Handler;

impl wasip2::exports::http::incoming_handler::Guest for Handler {
    fn handle(request: IncomingRequest, out: ResponseOutparam) {
        let local = pishoo_sdk::identity::local();
        let caller = pishoo_sdk::identity::caller();
        let target = request.path_with_query().unwrap_or_else(|| "/".into());
        let path = target.split('?').next().unwrap();
        let public = matches!(request.method(), Method::Get)
            && matches!(path, "/profile" | "/profile/avatar" | "/intro/context")
            || matches!(request.method(), Method::Head) && path == "/intro/context";
        let result = if !public && caller.as_ref() != Some(&local) {
            Err(fail(403, "owner required"))
        } else if mutating(&request.method())
            && request
                .headers()
                .get("sec-fetch-site")
                .iter()
                .any(|v| v == b"cross-site")
        {
            Err(fail(403, "cross-origin request denied"))
        } else {
            dispatch(&request, &target, &local, caller.as_ref())
        };
        respond(out, result);
    }
}
impl wasip2::exports::cli::run::Guest for Handler {
    fn run() -> Result<(), ()> {
        let local = pishoo_sdk::identity::local();
        let workspace = database("workspace").map_err(|_| ())?;
        contact_jobs(&workspace, &local).map_err(|_| ())
    }
}
wasip2::http::proxy::export!(Handler);
wasip2::cli::command::export!(Handler);

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
    fail(status, format!("workspace storage: {error}"))
}
fn now() -> i64 {
    wasip2::clocks::wall_clock::now().seconds as i64
}
fn random_id() -> String {
    wasip2::random::random::get_random_bytes(32)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn base() -> String {
    "/api/workspace".to_owned()
}
fn message_path() -> String {
    "/api/chat/message".to_owned()
}
fn mutating(method: &Method) -> bool {
    matches!(
        method,
        Method::Post | Method::Put | Method::Patch | Method::Delete
    )
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
fn read(request: &IncomingRequest, limit: usize) -> Result<Vec<u8>, Failure> {
    let body = request
        .consume()
        .map_err(|_| fail(400, "body already consumed"))?;
    let mut bytes = Vec::new();
    body.stream()
        .map_err(|_| fail(400, "invalid body"))?
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| fail(400, e.to_string()))?;
    if bytes.len() > limit {
        return Err(fail(413, "body too large"));
    }
    Ok(bytes)
}
fn input(request: &IncomingRequest, allowed: &[&str]) -> Result<Value, Failure> {
    let data = read(request, 64 * 1024)?;
    let value: Value = serde_json::from_slice(&data).map_err(|_| fail(400, "invalid JSON"))?;
    let fields = value
        .as_object()
        .ok_or_else(|| fail(400, "expected object"))?;
    if fields.keys().any(|k| !allowed.contains(&k.as_str())) {
        return Err(fail(400, "unknown field"));
    }
    Ok(value)
}
fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, Failure> {
    value[key]
        .as_str()
        .ok_or_else(|| fail(400, format!("invalid {key}")))
}
fn json_reply(status: u16, value: Value) -> Result<Reply, Failure> {
    Ok((
        status,
        "application/json".into(),
        if status == 204 {
            Vec::new()
        } else {
            serde_json::to_vec(&value).unwrap()
        },
        Vec::new(),
    ))
}
fn respond(out: ResponseOutparam, result: Result<Reply, Failure>) {
    let (status, content_type, bytes, extra) = result.unwrap_or_else(|(status, message)| {
        (
            status,
            "application/json".into(),
            serde_json::to_vec(&json!({"error":message})).unwrap(),
            Vec::new(),
        )
    });
    let fields = Fields::new();
    fields
        .append("content-type", content_type.as_bytes())
        .unwrap();
    if !extra.iter().any(|(key, _)| key == "cache-control") {
        fields.append("cache-control", b"no-store").unwrap();
    }
    fields.append("x-content-type-options", b"nosniff").unwrap();
    for (key, value) in extra {
        let _ = fields.append(&key, &value);
    }
    let response = OutgoingResponse::new(fields);
    response.set_status_code(status).unwrap();
    let body = response.body().unwrap();
    ResponseOutparam::set(out, Ok(response));
    if let Ok(mut stream) = body.write() {
        for chunk in bytes.chunks(8192) {
            if stream.write_all(chunk).is_err() {
                return;
            }
        }
    }
    let _ = OutgoingBody::finish(body, None);
}

fn http(
    target: &str,
    method: Method,
    path: &str,
    payload: Option<&Value>,
) -> Result<Reply, Failure> {
    let fields = Fields::new();
    if payload.is_some() {
        fields.append("content-type", b"application/json").unwrap();
    }
    let request = OutgoingRequest::new(fields);
    request
        .set_method(&method)
        .map_err(|_| fail(400, "invalid HTTP method"))?;
    request
        .set_scheme(Some(&Scheme::Https))
        .map_err(|_| fail(400, "invalid HTTP scheme"))?;
    request
        .set_authority(Some(target))
        .map_err(|_| fail(400, "invalid HTTP target"))?;
    request
        .set_path_with_query(Some(path))
        .map_err(|_| fail(400, "invalid HTTP path"))?;
    let body = if payload.is_some() {
        Some(request.body().map_err(|_| fail(400, "invalid HTTP body"))?)
    } else {
        None
    };
    let options = RequestOptions::new();
    let _ = options.set_connect_timeout(Some(2_000_000_000));
    let _ = options.set_first_byte_timeout(Some(3_000_000_000));
    let _ = options.set_between_bytes_timeout(Some(3_000_000_000));
    let response = outgoing_handler::handle(request, Some(options))
        .map_err(|e| fail(502, format!("HTTP request: {e:?}")))?;
    if let Some(body) = body {
        let bytes = serde_json::to_vec(payload.unwrap()).unwrap();
        body.write()
            .map_err(|_| fail(502, "HTTP body closed"))?
            .write_all(&bytes)
            .map_err(|e| fail(502, e.to_string()))?;
        OutgoingBody::finish(body, None).map_err(|_| fail(502, "HTTP upload failed"))?;
    }
    response.subscribe().block();
    let response = response
        .get()
        .ok_or_else(|| fail(502, "HTTP response not ready"))?
        .map_err(|_| fail(502, "HTTP response consumed"))?
        .map_err(|e| fail(502, format!("HTTP response: {e:?}")))?;
    let status = response.status();
    let fields = response.headers();
    let content_type = fields
        .get("content-type")
        .first()
        .and_then(|v| String::from_utf8(v.clone()).ok())
        .unwrap_or_else(|| "application/json".into());
    let headers = fields.entries();
    let body = response
        .consume()
        .map_err(|_| fail(502, "HTTP response body unavailable"))?;
    let mut bytes = Vec::new();
    body.stream()
        .map_err(|_| fail(502, "HTTP body unavailable"))?
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| fail(502, e.to_string()))?;
    if bytes.len() > 1024 * 1024 {
        return Err(fail(502, "HTTP response too large"));
    }
    Ok((status, content_type, bytes, headers))
}
fn api(method: Method, path: &str, payload: Option<&Value>) -> Result<Value, Failure> {
    let (status, _, body, _) = http("~", method, path, payload)?;
    if status == 202 {
        return Err(fail(409, "native operation requires access approval"));
    }
    if status < 200 || status >= 300 {
        return Err(fail(status, String::from_utf8_lossy(&body).into_owned()));
    }
    if body.is_empty() {
        Ok(Value::Null)
    } else {
        serde_json::from_slice(&body).map_err(|_| fail(502, "invalid native JSON response"))
    }
}
fn encoded(value: &str) -> String {
    form_urlencoded::byte_serialize(value.as_bytes()).collect()
}
fn contact(name: &str) -> Result<Value, Failure> {
    api(
        Method::Get,
        &format!("/std/contact/{}", encoded(name)),
        None,
    )
}
fn contacts() -> Result<Vec<Value>, Failure> {
    let mut items = Vec::new();
    let mut p = 1;
    loop {
        let value = api(
            Method::Get,
            &format!("/std/contacts?page={p}&page_size=100&sort=name&order=asc"),
            None,
        )?;
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
fn rule(name: &str, path: &str, method: &str, effect: &str) -> Result<(), Failure> {
    api(
        Method::Patch,
        &format!("/std/acl/allow/{}", encoded(name)),
        Some(&json!({"api":path,"method":method,"effect":effect})),
    )?;
    Ok(())
}
fn selected(value: &Value) -> Result<bool, Failure> {
    let items = value
        .as_array()
        .ok_or_else(|| fail(400, "invalid capabilities"))?;
    if items.len() > 1 || items.iter().any(|v| v != "chat") {
        return Err(fail(400, "unknown or duplicate capability"));
    }
    Ok(items.iter().any(|v| v == "chat"))
}
fn requested(selected: bool) -> Value {
    if selected {
        json!({message_path():["POST"]})
    } else {
        json!({})
    }
}
fn offered(selected: bool) -> Value {
    if selected {
        json!({message_path():{"allow":["POST"],"review":[],"deny":[]}})
    } else {
        json!({})
    }
}

fn database(kind: &str) -> Result<Connection, Failure> {
    let mut db = Connection::open_with_flags_and_vfs(
        format!("/db/{kind}.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
            | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        "unix-dotfile",
    )
    .map_err(storage)?;
    db.busy_timeout(std::time::Duration::from_secs(2))
        .map_err(storage)?;
    db.pragma_update(None, "synchronous", "FULL")
        .map_err(storage)?;
    db.pragma_update(None, "foreign_keys", "ON")
        .map_err(storage)?;
    let version: i64 = db
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(storage)?;
    if version == 0 {
        // Native rules are ordinary API writes by the installing owner. They are
        // initialization only: reopening a Lib never resets user ACL changes.
        if kind == "workspace" {
            for path in [
                format!("{}/profile", base()),
                format!("{}/profile/avatar", base()),
            ] {
                rule("*?", &path, "GET", "allow")?;
            }
        }
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let version: i64 = tx
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(storage)?;
        if version == 0 {
            let count: i64 = tx
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
                    [],
                    |row| row.get(0),
                )
                .map_err(storage)?;
            if count != 0 {
                return Err(fail(500, "unrecognized workspace database"));
            }
            tx.execute_batch(include_str!("workspace.sql"))
                .map_err(storage)?;
            tx.pragma_update(None, "user_version", 1).map_err(storage)?;
        } else if version != 1 {
            return Err(fail(500, "unsupported database version"));
        }
        tx.commit().map_err(storage)?;
    } else if version != 1 {
        return Err(fail(500, "unsupported database version"));
    }
    Ok(db)
}

fn dispatch(
    request: &IncomingRequest,
    target: &str,
    local: &(String, String),
    caller: Option<&(String, String)>,
) -> Result<Reply, Failure> {
    let path = target.split('?').next().unwrap();
    let method = request.method();
    let mut db = database("workspace")?;
    match (path, method) {
        ("/intro/context", Method::Get | Method::Head) => {
            let profile = profile(&db, local)?;
            let mut reply = json_reply(200, json!({
                "identity_name": local.0,
                "display_name": profile["display_name"],
                "avatar_url": profile["avatar_url"],
                "updated_at": profile["updated_at"],
                "viewer_name": caller.map(|identity| &identity.0),
                "is_owner": caller == Some(local),
            }))?;
            reply.3.push(("cache-control".into(), b"private, no-store".to_vec()));
            if matches!(request.method(), Method::Head) {
                reply.2.clear();
            }
            Ok(reply)
        }
        ("/context", Method::Get) => {
            let reviews = api(Method::Get, "/std/acl/reviews?page=1&page_size=1", None)?;
            let incoming = capability_requests(&db, false)?.len();
            json_reply(
                200,
                json!({"profile":local.0,"owner_name":local.0,"badges":{"pending_reviews":reviews["total"],"incoming_contacts":incoming}}),
            )
        }
        ("/capabilities", Method::Get) => json_reply(
            200,
            json!([
                {"id":"public_profile","version":"1","visibility":"public","approval_mode":"none","selectable":false,"endpoints":[{"method":"GET","path":format!("{}/profile",base())},{"method":"GET","path":format!("{}/profile/avatar",base())}]},
                {"id":"chat","version":"1","visibility":"contact","approval_mode":"capability","selectable":true,"endpoints":[{"method":"POST","path":message_path()}]}
            ]),
        ),
        ("/profile", Method::Get) => json_reply(200, profile(&db, local)?),
        ("/settings/profile", Method::Get) => json_reply(200, profile(&db, local)?),
        ("/settings/profile", Method::Patch) => {
            let doc = input(request, &["display_name"])?;
            let display = string(&doc, "display_name")?.trim();
            if display.chars().count() > 80 || display.chars().any(char::is_control) {
                return Err(fail(400, "invalid display name"));
            }
            db.execute(
                "UPDATE profile_preferences SET display_name=?1,updated_at=?2 WHERE id=1",
                params![
                    if display.is_empty() {
                        None
                    } else {
                        Some(display)
                    },
                    now()
                ],
            )
            .map_err(storage)?;
            json_reply(200, profile(&db, local)?)
        }
        ("/profile/avatar" | "/settings/profile/avatar", Method::Get) => avatar(&db, request),
        ("/settings/profile/avatar", Method::Put) => {
            let bytes = read(request, 1024 * 1024)?;
            let media = request
                .headers()
                .get("content-type")
                .first()
                .and_then(|v| String::from_utf8(v.clone()).ok())
                .unwrap_or_default();
            let kind = imagesize::image_type(&bytes).map_err(|_| fail(415, "invalid avatar"))?;
            let extension = match (kind, media.as_str()) {
                (imagesize::ImageType::Jpeg, "image/jpeg") => "jpg",
                (imagesize::ImageType::Png, "image/png") => "png",
                (imagesize::ImageType::Webp, "image/webp") => "webp",
                _ => return Err(fail(415, "invalid avatar media type")),
            };
            let size = imagesize::blob_size(&bytes).map_err(|_| fail(415, "invalid avatar"))?;
            if size.width == 0 || size.height == 0 || size.width > 2048 || size.height > 2048 {
                return Err(fail(400, "invalid avatar dimensions"));
            }
            std::fs::create_dir_all("/db/profile").map_err(|e| fail(500, e.to_string()))?;
            let file = format!("{:x}.{extension}", Sha256::digest(&bytes));
            let temporary = format!("/db/profile/.upload-{}", random_id());
            let mut output = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(|e| fail(500, e.to_string()))?;
            output
                .write_all(&bytes)
                .map_err(|e| fail(500, e.to_string()))?;
            output.sync_all().map_err(|e| fail(500, e.to_string()))?;
            drop(output);
            std::fs::rename(&temporary, format!("/db/profile/{file}"))
                .map_err(|e| fail(500, e.to_string()))?;
            db.execute(
                "UPDATE profile_preferences SET avatar_name=?1,updated_at=?2 WHERE id=1",
                params![file, now()],
            )
            .map_err(storage)?;
            json_reply(200, profile(&db, local)?)
        }
        ("/settings/profile/avatar", Method::Delete) => {
            db.execute(
                "UPDATE profile_preferences SET avatar_name=NULL,updated_at=?1 WHERE id=1",
                [now()],
            )
            .map_err(storage)?;
            json_reply(200, profile(&db, local)?)
        }
        ("/profiles", Method::Get) => {
            let name = target_name(target)?;
            let (status, _, body, _) =
                http(&name, Method::Get, &format!("{}/profile", base()), None)?;
            if status != 200 {
                return Err(fail(status, "remote profile unavailable"));
            }
            let mut profile: Value =
                serde_json::from_slice(&body).map_err(|_| fail(502, "invalid remote profile"))?;
            if !profile["avatar_url"].is_null() {
                profile["avatar_url"] = json!(format!(
                    "{}/profiles/avatar?name={}",
                    base(),
                    encoded(&name)
                ));
            }
            json_reply(200, profile)
        }
        ("/profiles/avatar", Method::Get) => http(
            &target_name(target)?,
            Method::Get,
            &format!("{}/profile/avatar", base()),
            None,
        ),
        ("/contact-directory", Method::Get) => json_reply(200, directory(&db)?),
        ("/contacts/saved", Method::Put) => {
            let name = target_name(target)?;
            let contact = contact(&name)?;
            db.execute("INSERT INTO saved_contacts(contact_name,contact_id,subject_id,saved_at) VALUES(?1,?2,?3,?4) ON CONFLICT(contact_name) DO UPDATE SET contact_id=excluded.contact_id,subject_id=excluded.subject_id,saved_at=excluded.saved_at",params![name,contact["id"].as_i64().ok_or_else(||fail(502,"missing contact id"))?,string(&contact,"subject_id")?.as_bytes(),now()]).map_err(storage)?;
            json_reply(204, Value::Null)
        }
        ("/contacts/saved", Method::Delete) => {
            db.execute(
                "DELETE FROM saved_contacts WHERE contact_name=?1",
                [target_name(target)?],
            )
            .map_err(storage)?;
            json_reply(204, Value::Null)
        }
        ("/capability-requests", Method::Get) => {
            json_reply(200, Value::Array(capability_requests(&db, false)?))
        }
        (
            "/contacts/capabilities/grant"
            | "/contacts/capabilities/deny"
            | "/contacts/capabilities/revoke",
            Method::Post,
        ) => http("~", Method::Post, &format!("/api/chat{target}"), None),
        ("/approvals", Method::Get) => json_reply(200, approvals(&db, target)?),
        ("/approvals", Method::Delete) => {
            let kind = required(target, "kind")?;
            let id = integer(target, "id", 0)?;
            match kind.as_str() {
                "access" => {
                    api(Method::Delete, &format!("/std/acl/review/{id}"), None)?;
                }
                "capability" => {
                    api(Method::Delete, &format!("/api/chat{target}"), None)?;
                }
                _ => return Err(fail(400, "invalid approval kind")),
            }
            json_reply(204, Value::Null)
        }
        ("/contact-requests", Method::Post) => {
            let doc = input(
                request,
                &[
                    "target_name",
                    "description",
                    "requested_capabilities",
                    "offered_capabilities",
                ],
            )?;
            let name = name(string(&doc, "target_name")?)?;
            if name == local.0 {
                return Err(fail(400, "cannot contact yourself"));
            }
            let description = string(&doc, "description")?;
            if description.chars().count() > 80 || description.chars().any(char::is_control) {
                return Err(fail(400, "invalid description"));
            }
            selected(&doc["requested_capabilities"])?;
            selected(&doc["offered_capabilities"])?;
            expire_requests(&db)?;
            let tx = db
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage)?;
            let existing:Option<i64>=tx.query_row("SELECT id FROM outbound_contact_requests WHERE target_name=?1 AND status IN ('queued','pending')",[&name],|row|row.get(0)).optional().map_err(storage)?;
            if existing.is_some() {
                return Err(fail(409, "contact request already pending"));
            }
            let time = now();
            tx.execute("INSERT INTO outbound_contact_requests(target_name,description,requested_capabilities,offered_capabilities,application_id,status,expired_after,delivery_deadline,next_attempt_at,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,'queued',?6,?6,?7,?7,?7)",params![name,description,doc["requested_capabilities"].to_string(),doc["offered_capabilities"].to_string(),random_id(),time+7*86400,time]).map_err(storage)?;
            let id = tx.last_insert_rowid();
            tx.commit().map_err(storage)?;
            json_reply(201, outbound_request(&db, id)?)
        }
        ("/contact-requests", Method::Get) => {
            expire_requests(&db)?;
            if let Some(id) = parameter(target, "id")? {
                return json_reply(
                    200,
                    outbound_request(&db, id.parse().map_err(|_| fail(400, "invalid id"))?)?,
                );
            }
            let (p, size) = page(target)?;
            let total: i64 = db
                .query_row("SELECT count(*) FROM outbound_contact_requests", [], |r| {
                    r.get(0)
                })
                .map_err(storage)?;
            let mut statement = db
                .prepare(
                    "SELECT id FROM outbound_contact_requests ORDER BY id DESC LIMIT ?1 OFFSET ?2",
                )
                .map_err(storage)?;
            let ids = statement
                .query_map(params![size, (p - 1).saturating_mul(size)], |r| {
                    r.get::<_, i64>(0)
                })
                .map_err(storage)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(storage)?;
            let items = ids
                .into_iter()
                .map(|id| outbound_request(&db, id))
                .collect::<Result<Vec<_>, _>>()?;
            json_reply(
                200,
                json!({"items":items,"total":total,"page":p,"page_size":size}),
            )
        }
        ("/contact-requests", Method::Delete) => {
            let id = integer(target, "id", 0)?;
            let count=db.execute("DELETE FROM outbound_contact_requests WHERE id=?1 AND (lease_until IS NULL OR lease_until<=?2)",params![id,now()]).map_err(storage)?;
            if count == 0 {
                return Err(fail(409, "request not found or in progress"));
            }
            json_reply(204, Value::Null)
        }
        ("/contact-requests/refresh", Method::Post) => {
            let id = integer(target, "id", 0)?;
            db.execute("UPDATE outbound_contact_requests SET next_attempt_at=?1 WHERE id=?2 AND status IN ('queued','pending','active')",params![now(),id]).map_err(storage)?;
            json_reply(200, outbound_request(&db, id)?)
        }
        _ => Err(fail(404, "operation not found")),
    }
}

fn profile(db: &Connection, local: &(String, String)) -> Result<Value, Failure> {
    db.query_row("SELECT display_name,avatar_name,updated_at FROM profile_preferences WHERE id=1",[],|row|Ok(json!({"identity_name":local.0,"subject_id":local.1,"display_name":row.get::<_,Option<String>>(0)?,"avatar_url":row.get::<_,Option<String>>(1)?.map(|_|format!("{}/profile/avatar",base())),"updated_at":row.get::<_,i64>(2)?}))).map_err(storage)
}
fn avatar(db: &Connection, request: &IncomingRequest) -> Result<Reply, Failure> {
    let file: Option<String> = db
        .query_row(
            "SELECT avatar_name FROM profile_preferences WHERE id=1",
            [],
            |row| row.get(0),
        )
        .map_err(storage)?;
    let file = file.ok_or_else(|| fail(404, "avatar not found"))?;
    if file.contains('/') || file.contains("..") {
        return Err(fail(500, "invalid avatar path"));
    }
    let etag = format!("\"{file}\"");
    if request
        .headers()
        .get("if-none-match")
        .iter()
        .any(|v| v == etag.as_bytes())
    {
        return Ok((
            304,
            "image/jpeg".into(),
            Vec::new(),
            vec![("etag".into(), etag.into_bytes())],
        ));
    }
    let media = if file.ends_with(".png") {
        "image/png"
    } else if file.ends_with(".webp") {
        "image/webp"
    } else {
        "image/jpeg"
    };
    let bytes =
        std::fs::read(format!("/db/profile/{file}")).map_err(|_| fail(404, "avatar not found"))?;
    Ok((
        200,
        media.into(),
        bytes,
        vec![("etag".into(), etag.into_bytes())],
    ))
}
fn outbound_request(db: &Connection, id: i64) -> Result<Value, Failure> {
    db.query_row("SELECT id,target_name,description,requested_capabilities,offered_capabilities,status,expired_after,delivery_deadline,remote_expired_after,last_checked_at,error_message,created_at,updated_at FROM outbound_contact_requests WHERE id=?1",[id],|r|Ok(json!({"id":r.get::<_,i64>(0)?,"target_name":r.get::<_,String>(1)?,"description":r.get::<_,String>(2)?,"requested_capabilities":serde_json::from_str::<Value>(&r.get::<_,String>(3)?).unwrap_or(Value::Null),"offered_capabilities":serde_json::from_str::<Value>(&r.get::<_,String>(4)?).unwrap_or(Value::Null),"status":r.get::<_,String>(5)?,"expired_after":r.get::<_,i64>(6)?,"delivery_deadline":r.get::<_,i64>(7)?,"remote_expired_after":r.get::<_,Option<i64>>(8)?,"last_checked_at":r.get::<_,Option<i64>>(9)?,"error_message":r.get::<_,Option<String>>(10)?,"created_at":r.get::<_,i64>(11)?,"updated_at":r.get::<_,i64>(12)?}))).optional().map_err(storage)?.ok_or_else(||fail(404,"contact request not found"))
}
fn capability_requests(_db: &Connection, expired: bool) -> Result<Vec<Value>, Failure> {
    let value = api(
        Method::Get,
        &format!(
            "/api/chat/capability-requests?status={}",
            if expired { "expired" } else { "pending" }
        ),
        None,
    )?;
    value
        .as_array()
        .cloned()
        .ok_or_else(|| fail(502, "invalid Chat capability list"))
}
fn approvals(db: &Connection, target: &str) -> Result<Value, Failure> {
    let status = parameter(target, "status")?.unwrap_or_else(|| "pending".into());
    if !matches!(status.as_str(), "pending" | "expired") {
        return Err(fail(400, "invalid approval status"));
    }
    let mut items = capability_requests(db, status == "expired")?;
    let mut p = 1;
    loop {
        let value = api(
            Method::Get,
            &format!("/std/acl/reviews?status={status}&page={p}&page_size=100"),
            None,
        )?;
        let values = value["items"]
            .as_array()
            .ok_or_else(|| fail(502, "invalid review list"))?;
        for value in values {
            let mut value = value.clone();
            value["kind"] = json!("access");
            // daccess uses ISO dates for review expiry; Workspace displays seconds.
            if let Some(date) = value["expired_after"].as_str() {
                value["expired_after"] = json!(iso_seconds(date)?);
            }
            items.push(value);
        }
        if p * 100 >= value["total"].as_i64().unwrap_or(0) || values.is_empty() {
            break;
        }
        p += 1;
    }
    let key = if status == "expired" {
        "expired_after"
    } else {
        "requested_at"
    };
    items.sort_by_key(|item| std::cmp::Reverse(item[key].as_i64().unwrap_or(0)));
    let total = items.len();
    let (p, size) = page(target)?;
    let items = items
        .into_iter()
        .skip((p - 1).saturating_mul(size).min(i64::MAX) as usize)
        .take(size as usize)
        .collect::<Vec<_>>();
    Ok(json!({"items":items,"total":total,"page":p,"page_size":size}))
}
fn iso_seconds(value: &str) -> Result<i64, Failure> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|date| date.timestamp())
        .map_err(|_| fail(502, "invalid review timestamp"))
}
fn directory(db: &Connection) -> Result<Value, Failure> {
    let mut items = Vec::new();
    for c in contacts()? {
        let saved: Option<(i64, Vec<u8>)> = db
            .query_row(
                "SELECT contact_id,subject_id FROM saved_contacts WHERE contact_name=?1",
                [string(&c, "name")?],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(storage)?;
        let saved = saved.is_some_and(|(id, s)| {
            c["id"] == id && s == c["subject_id"].as_str().unwrap_or_default().as_bytes()
        });
        let capability = api(
            Method::Get,
            &format!(
                "/api/chat/chat/capability?name={}",
                encoded(string(&c, "name")?)
            ),
            None,
        )?;
        if saved || capability["can_send"] == true || capability["can_receive"] == true {
            items.push(json!({"name":c["name"],"saved":saved,"chat_available":capability["can_send"]==true||capability["can_receive"]==true,"remote_chat_granted":capability["remote_grant"]}));
        }
    }
    Ok(Value::Array(items))
}

fn expire_requests(db: &Connection) -> Result<(), Failure> {
    db.execute("UPDATE outbound_contact_requests SET status=CASE WHEN status='queued' THEN 'failed' ELSE 'expired' END,lease_until=NULL,updated_at=?1,error_message='contact request expired' WHERE status IN ('queued','pending') AND expired_after<=?1",[now()]).map_err(storage)?;
    Ok(())
}
fn contact_jobs(db: &Connection, local: &(String, String)) -> Result<(), Failure> {
    expire_requests(db)?;
    // Claim one bounded unit of work, persist the lease before external I/O.
    // No lock or SQLite transaction is held during an HTTP operation.
    db.execute_batch("BEGIN IMMEDIATE").map_err(storage)?;
    let job:Option<(i64,String,String,String,String,String,String)>=db.query_row("SELECT id,target_name,application_id,description,requested_capabilities,offered_capabilities,status FROM outbound_contact_requests WHERE status IN ('queued','pending','active') AND next_attempt_at<=?1 AND (status='active' OR expired_after>?1) AND (lease_until IS NULL OR lease_until<=?1) ORDER BY next_attempt_at,id LIMIT 1",[now()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?))).optional().map_err(storage)?;
    let lease = now() + 120;
    if let Some((id, ..)) = &job {
        db.execute(
            "UPDATE outbound_contact_requests SET lease_until=?1 WHERE id=?2",
            params![lease, id],
        )
        .map_err(storage)?;
    }
    db.execute_batch("COMMIT").map_err(storage)?;
    let Some((id, target, application_id, description, requested_caps, offered_caps, status)) = job
    else {
        return Ok(());
    };
    let outcome = (|| {
        let requests: Value = serde_json::from_str(&requested_caps)
            .map_err(|_| fail(500, "invalid stored capabilities"))?;
        let offers: Value = serde_json::from_str(&offered_caps)
            .map_err(|_| fail(500, "invalid stored capabilities"))?;
        let request_chat = selected(&requests)?;
        let offer_chat = selected(&offers)?;
        let (method, path, body) = if status == "queued" {
            (
                Method::Post,
                "/std/contact".into(),
                Some(
                    json!({"application_id":application_id,"description":description,"class":"Human","requested_access":requested(request_chat),"offers":offered(offer_chat)}),
                ),
            )
        } else {
            (
                Method::Get,
                format!("/std/contact/self?application_id={application_id}"),
                None,
            )
        };
        let (status, _, bytes, _) = http(&target, method, &path, body.as_ref())?;
        if status == 202 {
            return Err(fail(409, "remote access approval pending"));
        }
        if status != 200 && status != 201 {
            return Err(fail(status, "remote contact request rejected"));
        }
        let remote: Value =
            serde_json::from_slice(&bytes).map_err(|_| fail(502, "invalid contact receipt"))?;
        if remote["application_id"] != application_id || remote["name"] != target {
            return Err(fail(502, "contact receipt does not match request"));
        }
        let sid = string(&remote, "subject_id")?;
        if sid.len() != 64 || !sid.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(fail(502, "invalid contact identity"));
        }
        if remote["status"] == "active" {
            let c = match contact(&target) {
                Ok(c) => c,
                Err((404, _)) => {
                    api(
                        Method::Put,
                        &format!("/std/contact/{}", encoded(&target)),
                        Some(
                            &json!({"subject_id":sid,"class":"Human","description":"","status":"active","requested_access":requested(request_chat),"offers":offered(offer_chat)}),
                        ),
                    )?;
                    contact(&target)?
                }
                Err(error) => return Err(error),
            };
            if matches!(c["status"].as_str(), Some("pending" | "transfered")) {
                api(
                    Method::Patch,
                    &format!("/std/contact/{}", encoded(&target)),
                    Some(&json!({"status":"active"})),
                )?;
            }
            if c["status"] == "blocked" || c["status"] == "expired" {
                return Err(fail(409, "local contact blocks reconciliation"));
            }
            if offer_chat {
                api(
                    Method::Post,
                    &format!(
                        "/api/chat/contacts/capabilities/grant?name={}&capability=chat",
                        encoded(&target)
                    ),
                    None,
                )?;
            }
        }
        if remote["status"] == "pending"
            && remote["expired_after"]
                .as_i64()
                .is_none_or(|expires| expires <= now())
        {
            return Err(fail(410, "remote request expired"));
        }
        Ok(remote)
    })();
    match outcome {
        Ok(remote) => {
            let state = string(&remote, "status")?;
            if !matches!(
                state,
                "pending" | "active" | "denied" | "expired" | "revoked"
            ) {
                return Err(fail(502, "unknown contact status"));
            }
            let expiry = remote["expired_after"]
                .as_i64()
                .ok_or_else(|| fail(502, "missing contact expiry"))?;
            db.execute("UPDATE outbound_contact_requests SET status=?1,remote_expired_after=?2,expired_after=CASE WHEN ?1='pending' THEN min(delivery_deadline,?2) ELSE expired_after END,last_checked_at=?3,next_attempt_at=?4,attempt_count=attempt_count+1,error_message=NULL,lease_until=NULL,updated_at=?3 WHERE id=?5 AND lease_until=?6",params![state,expiry,now(),now()+if state=="active" {30}else{3},id,lease]).map_err(storage)?;
        }
        Err((code, error)) => {
            let terminal = matches!(code, 400 | 403 | 404 | 410);
            db.execute("UPDATE outbound_contact_requests SET status=CASE WHEN ?1 THEN 'failed' ELSE status END,attempt_count=attempt_count+1,error_message=?2,next_attempt_at=?3,lease_until=NULL,last_checked_at=?4,updated_at=?4 WHERE id=?5 AND lease_until=?6",params![terminal,error,now()+5,now(),id,lease]).map_err(storage)?;
        }
    }
    let _ = local;
    Ok(())
}
