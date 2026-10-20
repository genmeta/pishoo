//! Ordinary user Lib: business data in /db, native resources accessed via HTTP.
use std::io::{Read, Write};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
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

        let receiving = path == "/message" && matches!(request.method(), Method::Post);
        let result = if !receiving && caller.as_ref() != Some(&local) {
            Err(fail(403, "owner required"))
        } else if mutating(&request.method())
            && !receiving
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
        let chat = database("message").map_err(|_| ())?;
        message_jobs(&chat, &local).map_err(|_| ())
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
    fail(status, format!("chat storage: {error}"))
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
    "/std".to_owned()
}
fn message_path() -> String {
    format!("{}/message", base())
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
fn integer(target: &str, key: &str, default: i64) -> Result<i64, Failure> {
    parameter(target, key)?.map_or(Ok(default), |v| {
        v.parse().map_err(|_| fail(400, "invalid number"))
    })
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
    fields.append("cache-control", b"no-store").unwrap();
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
    fields
        .append("if-reviewed", b"break")
        .map_err(|_| fail(500, "invalid review mode"))?;
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
        &format!("/std/contact?name={}", encoded(name)),
        None,
    )
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
                return Err(fail(500, "unrecognized workspace database"));
            }
            tx.execute_batch(include_str!("message.sql"))
                .map_err(storage)?;
            tx.pragma_update(None, "user_version", 2).map_err(storage)?;
        } else if version == 1 {
            tx.execute_batch(
                "ALTER TABLE chat_messages ADD COLUMN media_type TEXT NOT NULL DEFAULT 'text/plain' CHECK (length(media_type) BETWEEN 1 AND 127)",
            )
            .map_err(storage)?;
            tx.pragma_update(None, "user_version", 2).map_err(storage)?;
        } else if version != 2 {
            return Err(fail(500, "unsupported database version"));
        }
        tx.commit().map_err(storage)?;
    } else if version == 1 {
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        tx.execute_batch(
            "ALTER TABLE chat_messages ADD COLUMN media_type TEXT NOT NULL DEFAULT 'text/plain' CHECK (length(media_type) BETWEEN 1 AND 127)",
        )
        .map_err(storage)?;
        tx.pragma_update(None, "user_version", 2).map_err(storage)?;
        tx.commit().map_err(storage)?;
    } else if version != 2 {
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
    let mut chat = database("message")?;
    match (path, method) {
        ("/message/list", Method::Get) => {
            let contact_name = parameter(target, "name")?
                .map(|value| name(&value))
                .transpose()?;
            if let Some(contact_name) = &contact_name {
                contact(contact_name)?;
            }
            let limit = integer(target, "limit", 50)?;
            if !(1..=100).contains(&limit) {
                return Err(fail(400, "invalid limit"));
            }
            let after = integer(target, "after", 0)?;
            let mut stmt=chat.prepare("SELECT id FROM chat_messages WHERE (?1 IS NULL OR contact_name=?1) AND id>?2 ORDER BY id LIMIT ?3").map_err(storage)?;
            let ids = stmt
                .query_map(params![contact_name, after, limit + 1], |r| {
                    r.get::<_, i64>(0)
                })
                .map_err(storage)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(storage)?;
            let more = ids.len() > limit as usize;
            let ids = &ids[..ids.len().min(limit as usize)];
            let items = ids
                .iter()
                .map(|id| local_message(&chat, *id))
                .collect::<Result<Vec<_>, _>>()?;
            json_reply(
                200,
                json!({"items":items,"next_cursor":if more {ids.last().map(i64::to_string)} else {None}}),
            )
        }
        ("/message", Method::Put) => {
            let doc = input(
                request,
                &["to", "content", "media_type", "client_message_id"],
            )?;
            let contact_name = name(string(&doc, "to")?)?;
            let c = contact(&contact_name)?;
            if c["status"] == "blocked" {
                return Err(fail(403, "chat permission unavailable"));
            }
            let text = validated_content(&doc)?;
            let media_type = validated_media_type(&doc)?;
            let client_id = string(&doc, "client_message_id")?.to_owned();
            if client_id.is_empty()
                || client_id.len() > 128
                || !client_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                return Err(fail(400, "invalid client_message_id"));
            }
            let time = now();
            let tx = chat
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage)?;
            tx.execute("INSERT INTO chat_conversations(contact_name,updated_at) VALUES(?1,?2) ON CONFLICT(contact_name) DO UPDATE SET updated_at=excluded.updated_at",params![contact_name,time]).map_err(storage)?;
            let existing: Option<(i64, String, String, String)> = tx.query_row("SELECT id,text,media_type,direction FROM chat_messages WHERE contact_name=?1 AND client_message_id=?2",params![contact_name,client_id],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).optional().map_err(storage)?;
            let (id, created) = if let Some((id, old_text, old_media_type, direction)) = existing {
                if direction != "outgoing" || old_text != text || old_media_type != media_type {
                    return Err(fail(409, "message id reused with different content"));
                }
                (id, false)
            } else {
                tx.execute("INSERT INTO chat_messages(contact_name,client_message_id,direction,state,sender_name,recipient_name,text,media_type,created_at,updated_at,next_attempt_at) VALUES(?1,?2,'outgoing','queued',?3,?1,?4,?5,?6,?6,?6)",params![contact_name,client_id,local.0,text,media_type,time]).map_err(storage)?;
                (tx.last_insert_rowid(), true)
            };
            if created {
                tx.execute("INSERT INTO chat_jobs(kind,contact_name,message_id,state,available_at,created_at,updated_at) VALUES('send',?1,?2,'queued',?3,?3,?3)",params![contact_name,id,time]).map_err(storage)?;
            }
            tx.commit().map_err(storage)?;
            json_reply(if created { 202 } else { 200 }, local_message(&chat, id)?)
        }
        ("/message", Method::Post) => {
            let caller = caller.ok_or_else(|| fail(403, "verified sender required"))?;
            let doc = input(request, &["client_message_id", "content", "media_type"])?;
            let client_id = string(&doc, "client_message_id")?;
            if client_id.is_empty()
                || client_id.len() > 128
                || !client_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                return Err(fail(400, "invalid client_message_id"));
            }
            let content = validated_content(&doc)?;
            let media_type = validated_media_type(&doc)?;
            let time = now();
            let tx = chat
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage)?;
            let old:Option<(i64,String,String,String)>=tx.query_row("SELECT id,text,media_type,direction FROM chat_messages WHERE contact_name=?1 AND client_message_id=?2",params![caller.0,client_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional().map_err(storage)?;
            let (id, created) = if let Some((id, original, original_media_type, direction)) = old {
                if direction != "incoming"
                    || original != content
                    || original_media_type != media_type
                {
                    return Err(fail(409, "message id reused with different content"));
                }
                (id, false)
            } else {
                tx.execute("INSERT INTO chat_conversations(contact_name,updated_at) VALUES(?1,?2) ON CONFLICT(contact_name) DO UPDATE SET updated_at=excluded.updated_at",params![caller.0,time]).map_err(storage)?;
                tx.execute("INSERT INTO chat_messages(contact_name,client_message_id,direction,state,sender_name,recipient_name,text,media_type,created_at,updated_at) VALUES(?1,?2,'incoming','received',?1,?3,?4,?5,?6,?6)",params![caller.0,client_id,local.0,content,media_type,time]).map_err(storage)?;
                (tx.last_insert_rowid(), true)
            };
            tx.commit().map_err(storage)?;
            let item = local_message(&chat, id)?;
            json_reply(if created { 201 } else { 200 }, item)
        }
        ("/message/retry", Method::Put) => {
            let id = retry_index(target)?;
            let contact_name: String = chat
                .query_row(
                    "SELECT contact_name FROM chat_messages WHERE id=?1 AND direction='outgoing'",
                    [id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(storage)?
                .ok_or_else(|| fail(404, "message not found"))?;
            if contact(&contact_name)?["status"] == "blocked" {
                return Err(fail(403, "chat permission unavailable"));
            }
            let tx = chat
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage)?;
            let time = now();
            let changed = tx
                .execute(
                    "UPDATE chat_messages SET state='queued',error_message=NULL,updated_at=?1,next_attempt_at=?1 WHERE id=?2 AND direction='outgoing' AND state IN ('failed','blocked')",
                    params![time, id],
                )
                .map_err(storage)?;
            if changed == 0 {
                return Err(fail(409, "message cannot be retried"));
            }
            tx.execute(
                "UPDATE chat_jobs SET state='queued',available_at=?1,lease_token=NULL,lease_until=NULL,updated_at=?1 WHERE message_id=?2",
                params![time, id],
            )
            .map_err(storage)?;
            tx.commit().map_err(storage)?;
            json_reply(200, local_message(&chat, id)?)
        }
        _ => Err(fail(404, "operation not found")),
    }
}

fn validated_content(doc: &Value) -> Result<&str, Failure> {
    let text = string(doc, "content")?.trim();
    if text.is_empty()
        || text.chars().count() > 4000
        || text
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err(fail(400, "invalid message content"));
    }
    Ok(text)
}
fn validated_media_type(doc: &Value) -> Result<&str, Failure> {
    let media_type = string(doc, "media_type")?.trim();
    if media_type.is_empty()
        || media_type.len() > 127
        || media_type
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        || !media_type.contains('/')
    {
        return Err(fail(400, "invalid media_type"));
    }
    Ok(media_type)
}
fn local_message(db: &Connection, id: i64) -> Result<Value, Failure> {
    db.query_row(
        "SELECT id,client_message_id,remote_message_id,direction,state,sender_name,recipient_name,text,media_type,created_at,updated_at,error_message FROM chat_messages WHERE id=?1",
        [id],
        |row| {
            let state: String = row.get(4)?;
            let error: Option<String> = row.get(11)?;
            Ok(json!({
                "index": row.get::<_, i64>(0)?,
                "client_message_id": row.get::<_, String>(1)?,
                "remote_message_id": row.get::<_, Option<String>>(2)?,
                "direction": row.get::<_, String>(3)?,
                "state": delivery_state(&state, error.as_deref()),
                "from": row.get::<_, String>(5)?,
                "to": row.get::<_, String>(6)?,
                "content": row.get::<_, String>(7)?,
                "media_type": row.get::<_, String>(8)?,
                "created_at": row.get::<_, i64>(9)?,
                "updated_at": row.get::<_, i64>(10)?,
                "error_message": error,
            }))
        },
    ).map_err(storage)
}

fn retry_index(target: &str) -> Result<i64, Failure> {
    let value = parameter(target, "index")?.ok_or_else(|| fail(400, "missing index"))?;
    let index = value
        .parse::<i64>()
        .map_err(|_| fail(400, "invalid index"))?;
    if index <= 0 {
        return Err(fail(400, "invalid index"));
    }
    Ok(index)
}

fn message_jobs(chat: &Connection, local: &(String, String)) -> Result<(), Failure> {
    let time = now();
    chat.execute("UPDATE chat_jobs SET state='queued',lease_token=NULL,lease_until=NULL,available_at=?1 WHERE state='running' AND lease_until<=?1",[time]).map_err(storage)?;
    chat.execute_batch("BEGIN IMMEDIATE").map_err(storage)?;
    let job:Option<(i64,i64,String,i64)>=chat.query_row("SELECT id,message_id,contact_name,attempt_count FROM chat_jobs WHERE state='queued' AND available_at<=?1 ORDER BY available_at,id LIMIT 1",[time],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional().map_err(storage)?;
    let lease = random_id();
    if let Some((id, message, ..)) = &job {
        chat.execute("UPDATE chat_jobs SET state='running',lease_token=?1,lease_until=?2,updated_at=?3 WHERE id=?4",params![lease,time+120,time,id]).map_err(storage)?;
        chat.execute(
            "UPDATE chat_messages SET state='sending',last_attempt_at=?1,updated_at=?1 WHERE id=?2",
            params![time, message],
        )
        .map_err(storage)?;
    }
    chat.execute_batch("COMMIT").map_err(storage)?;
    let Some((job, message, target, attempt)) = job else {
        return Ok(());
    };
    let item = local_message(chat, message)?;
    let result = (|| {
        let c = contact(&target)?;
        if c["status"] == "blocked" {
            return Err(fail(403, "chat permission unavailable"));
        }
        let (status, _, bytes, _) = http(
            &target,
            Method::Post,
            &message_path(),
            Some(
                &json!({"client_message_id":item["client_message_id"],"content":item["content"],"media_type":item["media_type"]}),
            ),
        )?;
        if status != 200 && status != 201 {
            return Err(fail(
                status,
                if needs_review(status, &bytes) {
                    "need_review"
                } else {
                    "message delivery rejected"
                },
            ));
        }
        let envelope: Value =
            serde_json::from_slice(&bytes).map_err(|_| fail(502, "invalid message receipt"))?;
        if envelope["client_message_id"] != item["client_message_id"]
            || envelope["from"] != local.0
            || envelope["to"] != target
            || envelope["content"] != item["content"]
            || envelope["media_type"] != item["media_type"]
        {
            return Err(fail(502, "message receipt mismatch"));
        }
        let remote_index = envelope["index"]
            .as_i64()
            .filter(|index| *index > 0)
            .ok_or_else(|| fail(502, "invalid message receipt index"))?;
        Ok(remote_index.to_string())
    })();
    finish_delivery(chat, job, message, &lease, attempt, result, now())
}

fn finish_delivery(
    chat: &Connection,
    job: i64,
    message: i64,
    lease: &str,
    attempt: i64,
    result: Result<String, Failure>,
    time: i64,
) -> Result<(), Failure> {
    let tx = chat.unchecked_transaction().map_err(storage)?;
    match result {
        Ok(remote) => {
            let n=tx.execute("UPDATE chat_jobs SET state='completed',lease_token=NULL,lease_until=NULL,updated_at=?1 WHERE id=?2 AND lease_token=?3",params![time,job,lease]).map_err(storage)?;
            if n == 1 {
                tx.execute("UPDATE chat_messages SET state='sent',remote_message_id=?1,delivered_at=?2,error_message=NULL,updated_at=?2 WHERE id=?3",params![remote,time,message]).map_err(storage)?;
            }
        }
        Err((code, error)) => {
            let reviewing = error == "need_review";
            let permanent = !reviewing && matches!(code, 400 | 403 | 404 | 409);
            let dead = permanent || (!reviewing && attempt >= 7);
            let retry_at = time
                + if reviewing {
                    30
                } else {
                    5 * (1i64 << attempt.min(6))
                };
            let n=tx.execute("UPDATE chat_jobs SET state=?1,available_at=?2,attempt_count=attempt_count+CASE WHEN ?3='need_review' THEN 0 ELSE 1 END,last_error=?3,lease_token=NULL,lease_until=NULL,updated_at=?4 WHERE id=?5 AND lease_token=?6",params![if dead {"dead"} else {"queued"},retry_at,error,time,job,lease]).map_err(storage)?;
            if n == 1 {
                tx.execute("UPDATE chat_messages SET state=?1,error_message=?2,attempt_count=attempt_count+CASE WHEN ?2='need_review' THEN 0 ELSE 1 END,next_attempt_at=?3,updated_at=?4 WHERE id=?5",params![if code==403 {"blocked"} else if dead {"failed"} else {"queued"},error,retry_at,time,message]).map_err(storage)?;
            }
        }
    }
    tx.commit().map_err(storage)
}

fn needs_review(status: u16, bytes: &[u8]) -> bool {
    status == 400
        && serde_json::from_slice::<Value>(bytes).is_ok_and(|body| body["error"] == "need_review")
}

fn delivery_state<'a>(state: &'a str, error: Option<&str>) -> &'a str {
    if state == "queued" && error == Some("need_review") {
        "reviewing"
    } else {
        state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queued_delivery() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(include_str!("message.sql")).unwrap();
        db.execute("INSERT INTO chat_messages(id,contact_name,client_message_id,direction,state,sender_name,recipient_name,text,created_at,updated_at) VALUES(1,'peer.example','stable-id','outgoing','sending','owner.example','peer.example','Hello',1,1)", []).unwrap();
        db.execute("INSERT INTO chat_jobs(id,kind,contact_name,message_id,state,available_at,lease_token,lease_until,created_at,updated_at) VALUES(1,'send','peer.example',1,'running',1,'lease',120,1,1)", []).unwrap();
        db
    }

    #[test]
    fn message_resource_uses_numeric_indexes_and_requires_a_valid_retry_index() {
        let db = queued_delivery();
        let message = local_message(&db, 1).unwrap();
        assert_eq!(message["index"], 1);
        assert_eq!(message["from"], "owner.example");
        assert_eq!(message["to"], "peer.example");
        assert_eq!(message["content"], "Hello");
        assert_eq!(message["media_type"], "text/plain");

        assert_eq!(retry_index("/message/retry?index=42"), Ok(42));
        assert_eq!(
            retry_index("/message/retry?index=0"),
            Err(fail(400, "invalid index"))
        );
        assert_eq!(
            retry_index("/message/retry?index=invalid"),
            Err(fail(400, "invalid index"))
        );
        assert_eq!(
            retry_index("/message/retry"),
            Err(fail(400, "missing index"))
        );
    }

    #[test]
    fn review_waits_do_not_exhaust_retries_and_keep_the_message_id() {
        let db = queued_delivery();
        for time in 1..=10 {
            finish_delivery(&db, 1, 1, "lease", 0, Err(fail(400, "need_review")), time).unwrap();
            let message = local_message(&db, 1).unwrap();
            assert_eq!(message["state"], "reviewing");
            assert_eq!(message["client_message_id"], "stable-id");
            let job: (String, i64, i64) = db
                .query_row(
                    "SELECT state,attempt_count,available_at FROM chat_jobs WHERE id=1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap();
            assert_eq!(job, ("queued".into(), 0, time + 30));
            db.execute(
                "UPDATE chat_jobs SET state='running',lease_token='lease' WHERE id=1",
                [],
            )
            .unwrap();
        }
        finish_delivery(&db, 1, 1, "lease", 0, Ok("remote-id".into()), 11).unwrap();
        let message = local_message(&db, 1).unwrap();
        assert_eq!(message["state"], "sent");
        assert_eq!(message["client_message_id"], "stable-id");
        assert_eq!(message["remote_message_id"], "remote-id");
        assert!(message["error_message"].is_null());
    }

    #[test]
    fn ordinary_bad_requests_and_denials_end_delivery_and_stale_leases_cannot_change_it() {
        for (code, state) in [(400, "failed"), (403, "blocked")] {
            let db = queued_delivery();
            finish_delivery(&db, 1, 1, "stale", 0, Err(fail(400, "need_review")), 2).unwrap();
            assert_eq!(local_message(&db, 1).unwrap()["state"], "sending");
            finish_delivery(&db, 1, 1, "lease", 0, Err(fail(code, "rejected")), 3).unwrap();
            assert_eq!(local_message(&db, 1).unwrap()["state"], state);
            assert_eq!(
                db.query_row("SELECT state FROM chat_jobs WHERE id=1", [], |row| row
                    .get::<_, String>(
                    0
                ))
                .unwrap(),
                "dead"
            );
        }
        assert!(needs_review(400, br#"{"error":"need_review"}"#));
        assert!(!needs_review(400, br#"{"error":"invalid input"}"#));
        assert!(!needs_review(403, br#"{"error":"need_review"}"#));
    }
}
