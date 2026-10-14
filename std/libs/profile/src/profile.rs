//! Profile Lib: display information and avatars in its private /db directory.
use rusqlite::{Connection, TransactionBehavior, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use wasip2::http::types::*;

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
            && matches!(path, "/profile" | "/profile/avatar" | "/profile/context")
            || matches!(request.method(), Method::Head) && path == "/profile/context";
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
wasip2::http::proxy::export!(Handler);

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
    fail(status, format!("profile storage: {error}"))
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
fn mutating(method: &Method) -> bool {
    matches!(
        method,
        Method::Post | Method::Put | Method::Patch | Method::Delete
    )
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
                return Err(fail(500, "unrecognized profile database"));
            }
            tx.execute_batch(include_str!("profile.sql"))
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
    let db = database("profile")?;
    match (path, request.method()) {
        ("/profile/context", Method::Get | Method::Head) => {
            let profile = profile(&db, local)?;
            let mut reply = json_reply(
                200,
                json!({
                    "identity_name": local.0,
                    "display_name": profile["display_name"],
                    "avatar_url": profile["avatar_url"],
                    "updated_at": profile["updated_at"],
                    "viewer_name": caller.map(|identity| &identity.0),
                    "is_owner": caller == Some(local),
                }),
            )?;
            reply
                .3
                .push(("cache-control".into(), b"private, no-store".to_vec()));
            if matches!(request.method(), Method::Head) {
                reply.2.clear();
            }
            Ok(reply)
        }
        ("/profile", Method::Get) => json_reply(200, profile(&db, local)?),
        ("/profile", Method::Patch) => {
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
        ("/profile/avatar", Method::Get) => avatar(&db, request),
        ("/profile/avatar", Method::Put) => {
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
        ("/profile/avatar", Method::Delete) => {
            db.execute(
                "UPDATE profile_preferences SET avatar_name=NULL,updated_at=?1 WHERE id=1",
                [now()],
            )
            .map_err(storage)?;
            json_reply(200, profile(&db, local)?)
        }
        _ => Err(fail(404, "operation not found")),
    }
}

fn profile(db: &Connection, local: &(String, String)) -> Result<Value, Failure> {
    db.query_row("SELECT display_name,avatar_name,updated_at FROM profile_preferences WHERE id=1",[],|row|Ok(json!({"identity_name":local.0,"subject_id":local.1,"display_name":row.get::<_,Option<String>>(0)?,"avatar_url":row.get::<_,Option<String>>(1)?.map(|_|"/std/profile/avatar".to_owned()),"updated_at":row.get::<_,i64>(2)?}))).map_err(storage)
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
