//! Ordinary user Lib: business data in /db, native resources accessed via HTTP.
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
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
        observe_contacts(&chat, &chat).map_err(|_| ())?;
        message_jobs(&chat, &chat, &local).map_err(|_| ())
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
fn has_message(access: &Value) -> bool {
    access[message_path()]["allow"]
        .as_array()
        .is_some_and(|m| m.iter().any(|v| v == "POST" || v == "*"))
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
    let mut chat = database("message")?;
    match (path, method) {
        ("/message/context", Method::Get) => json_reply(
            200,
            json!({"owner_name":local.0,"capability":"message","endpoints":[{"method":"POST","path":message_path()}]}),
        ),
        ("/message/capability", Method::Get) => json_reply(
            200,
            chat_capability(&chat, &chat, &contact(&target_name(target)?)?)?,
        ),
        ("/message/messages", Method::Get) => {
            let name = target_name(target)?;
            contact(&name)?;
            let limit = integer(target, "limit", 50)?;
            if !(1..=100).contains(&limit) {
                return Err(fail(400, "invalid limit"));
            }
            let after = integer(target, "after", 0)?;
            let mut stmt=chat.prepare("SELECT id FROM chat_messages WHERE contact_name=?1 AND id>?2 ORDER BY id LIMIT ?3").map_err(storage)?;
            let ids = stmt
                .query_map(params![name, after, limit + 1], |r| r.get::<_, i64>(0))
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
        ("/message/messages", Method::Post) => {
            let name = target_name(target)?;
            let c = contact(&name)?;
            if chat_capability(&chat, &chat, &c)?["can_send"] != true {
                return Err(fail(403, "chat permission unavailable"));
            }
            let doc = input(request, &["text"])?;
            let text = validated_text(&doc)?;
            let time = now();
            let tx = chat
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage)?;
            tx.execute("INSERT INTO chat_conversations(contact_name,updated_at) VALUES(?1,?2) ON CONFLICT(contact_name) DO UPDATE SET updated_at=excluded.updated_at",params![name,time]).map_err(storage)?;
            tx.execute("INSERT INTO chat_messages(contact_name,client_message_id,direction,state,sender_name,recipient_name,text,created_at,updated_at,next_attempt_at) VALUES(?1,?2,'outgoing','queued',?3,?1,?4,?5,?5,?5)",params![name,random_id(),local.0,text,time]).map_err(storage)?;
            let id = tx.last_insert_rowid();
            tx.execute("INSERT INTO chat_jobs(kind,contact_name,message_id,state,available_at,created_at,updated_at) VALUES('send',?1,?2,'queued',?3,?3,?3)",params![name,id,time]).map_err(storage)?;
            tx.commit().map_err(storage)?;
            json_reply(202, local_message(&chat, id)?)
        }
        ("/message/messages/retry", Method::Post) => {
            let name = target_name(target)?;
            let id = integer(target, "id", 0)?;
            if chat_capability(&chat, &chat, &contact(&name)?)?["can_send"] != true {
                return Err(fail(403, "chat permission unavailable"));
            }
            let tx = chat
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage)?;
            let n=tx.execute("UPDATE chat_messages SET state='queued',error_message=NULL,updated_at=?1,next_attempt_at=?1 WHERE id=?2 AND contact_name=?3 AND direction='outgoing' AND state IN ('failed','blocked')",params![now(),id,name]).map_err(storage)?;
            if n == 0 {
                return Err(fail(409, "message cannot be retried"));
            }
            tx.execute("UPDATE chat_jobs SET state='queued',available_at=?1,lease_token=NULL,lease_until=NULL,updated_at=?1 WHERE message_id=?2",params![now(),id]).map_err(storage)?;
            tx.commit().map_err(storage)?;
            json_reply(200, local_message(&chat, id)?)
        }
        ("/message", Method::Post) => {
            let caller = caller.ok_or_else(|| fail(403, "verified sender required"))?;
            let c = api(Method::Get, "/std/contact/self", None)?;
            if c["status"] != "active" || c["name"] != caller.0 || c["subject_id"] != caller.1 {
                return Err(fail(403, "chat capability not approved"));
            }
            let doc = input(request, &["client_message_id", "text"])?;
            let client_id = string(&doc, "client_message_id")?;
            if client_id.is_empty()
                || client_id.len() > 128
                || !client_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                return Err(fail(400, "invalid client_message_id"));
            }
            let text = validated_text(&doc)?;
            let time = now();
            let tx = chat
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage)?;
            let old:Option<(i64,String)>=tx.query_row("SELECT id,text FROM chat_messages WHERE contact_name=?1 AND client_message_id=?2",params![caller.0,client_id],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(storage)?;
            let (id, created) = if let Some((id, original)) = old {
                if original != text {
                    return Err(fail(409, "message id reused with different text"));
                }
                (id, false)
            } else {
                tx.execute("INSERT INTO chat_conversations(contact_name,updated_at) VALUES(?1,?2) ON CONFLICT(contact_name) DO UPDATE SET updated_at=excluded.updated_at",params![caller.0,time]).map_err(storage)?;
                tx.execute("INSERT INTO chat_messages(contact_name,client_message_id,direction,state,sender_name,recipient_name,text,created_at,updated_at) VALUES(?1,?2,'incoming','received',?1,?3,?4,?5,?5)",params![caller.0,client_id,local.0,text,time]).map_err(storage)?;
                (tx.last_insert_rowid(), true)
            };
            tx.commit().map_err(storage)?;
            let item = local_message(&chat, id)?;
            json_reply(
                if created { 201 } else { 200 },
                json!({"id":item["id"],"client_message_id":client_id,"sender":caller.0,"recipient":local.0,"text":text,"created_at":item["created_at"]}),
            )
        }
        _ => Err(fail(404, "operation not found")),
    }
}

fn validated_text(doc: &Value) -> Result<&str, Failure> {
    let text = string(doc, "text")?.trim();
    if text.is_empty()
        || text.chars().count() > 4000
        || text
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err(fail(400, "invalid message text"));
    }
    Ok(text)
}
fn local_message(db: &Connection, id: i64) -> Result<Value, Failure> {
    db.query_row("SELECT id,client_message_id,remote_message_id,direction,state,sender_name,recipient_name,text,created_at,updated_at,error_message FROM chat_messages WHERE id=?1",[id],|r|Ok(json!({"id":r.get::<_,i64>(0)?.to_string(),"client_message_id":r.get::<_,String>(1)?,"remote_message_id":r.get::<_,Option<String>>(2)?,"direction":r.get::<_,String>(3)?,"state":r.get::<_,String>(4)?,"sender":r.get::<_,String>(5)?,"recipient":r.get::<_,String>(6)?,"text":r.get::<_,String>(7)?,"created_at":r.get::<_,i64>(8)?,"updated_at":r.get::<_,i64>(9)?,"error_message":r.get::<_,Option<String>>(10)?}))).map_err(storage)
}

fn remote_grant(chat: &Connection, c: &Value) -> Result<Option<bool>, Failure> {
    let state:Option<(Option<Vec<u8>>,bool)>=chat.query_row("SELECT subject_id,remote_message_granted FROM chat_capability_state WHERE contact_name=?1",[string(c,"name")?],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(storage)?;
    Ok(state
        .filter(|(s, _)| {
            s.as_deref() == Some(c["subject_id"].as_str().unwrap_or_default().as_bytes())
        })
        .map(|(_, grant)| grant))
}
fn observed(chat: &Connection, c: &Value, access: &Value) -> Result<(), Failure> {
    chat.execute("INSERT INTO chat_capability_state(contact_name,subject_id,remote_message_granted,updated_at) VALUES(?1,?2,?3,?4) ON CONFLICT(contact_name) DO UPDATE SET subject_id=excluded.subject_id,remote_message_granted=excluded.remote_message_granted,updated_at=excluded.updated_at",params![string(c,"name")?,string(c,"subject_id")?.as_bytes(),has_message(access),now()]).map_err(storage)?;
    Ok(())
}
fn chat_capability(_db: &Connection, chat: &Connection, c: &Value) -> Result<Value, Failure> {
    let active = c["status"] == "active";
    let mut remote = remote_grant(chat, c)?;
    // A freshly reconciled native contact may appear after this CLI batch's
    // observation. Fetch the actual peer grant before its first send, rather
    // than requiring the contact queue to write another Lib's private data.
    if active && remote.is_none() {
        let (status, _, body, _) =
            http(string(c, "name")?, Method::Get, "/std/contact/self", None)?;
        if status == 200 {
            let receipt: Value =
                serde_json::from_slice(&body).map_err(|_| fail(502, "invalid contact receipt"))?;
            observed(chat, c, &receipt["granted_access"])?;
        } else if matches!(status, 403 | 404) {
            observed(chat, c, &json!({}))?;
        } else {
            return Err(fail(status, "remote contact status unavailable"));
        }
        remote = remote_grant(chat, c)?;
    }
    // Contact records retain a grant snapshot. Native ACL writes are the current authority.
    let receive = if active {
        let rules = api(Method::Get, "/std/acl/allow", None)?;
        has_message(&rules[string(c, "name")?])
    } else {
        false
    };
    let send = active && remote == Some(true);
    Ok(
        json!({"capability":"message","status":if c["status"]=="blocked" {"blocked"} else if send||receive {"available"} else {"waiting"},"contact_status":c["status"],"can_send":send,"can_receive":receive,"remote_grant":remote,"endpoints":[{"method":"POST","path":message_path()}]}),
    )
}
fn observe_contacts(db: &Connection, chat: &Connection) -> Result<(), Failure> {
    let eligible = contacts()?
        .into_iter()
        .filter(|c| {
            c["status"] == "active"
                && (c["requested_access"].get(message_path()).is_some()
                    || c["offers"].get(message_path()).is_some())
        })
        .collect::<Vec<_>>();
    // Derive a fair selection from the clock rather than retaining an invocation
    // counter or restarting at the first failing contact on every Store.
    if !eligible.is_empty() {
        let c = &eligible[(now().max(0) as usize / 3) % eligible.len()];
        let (status, _, body, _) =
            http(string(c, "name")?, Method::Get, "/std/contact/self", None)?;
        if status == 200 {
            let value: Value = serde_json::from_slice(&body)
                .map_err(|_| fail(502, "invalid peer contact status"))?;
            observed(chat, c, &value["granted_access"])?;
        } else if status == 403 || status == 404 {
            observed(chat, c, &json!({}))?;
        }
    }
    let _ = db;
    Ok(())
}

fn message_jobs(
    db: &Connection,
    chat: &Connection,
    local: &(String, String),
) -> Result<(), Failure> {
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
        if chat_capability(db, chat, &c)?["can_send"] != true {
            return Err(fail(403, "chat permission unavailable"));
        }
        let (status, _, bytes, _) = http(
            &target,
            Method::Post,
            &message_path(),
            Some(&json!({"client_message_id":item["client_message_id"],"text":item["text"]})),
        )?;
        if status != 200 && status != 201 {
            return Err(fail(status, "message delivery rejected"));
        }
        let envelope: Value =
            serde_json::from_slice(&bytes).map_err(|_| fail(502, "invalid message receipt"))?;
        if envelope["client_message_id"] != item["client_message_id"]
            || envelope["sender"] != local.0
            || envelope["recipient"] != target
            || envelope["text"] != item["text"]
        {
            return Err(fail(502, "message receipt mismatch"));
        }
        Ok(string(&envelope, "id")?.to_owned())
    })();
    let tx = chat.unchecked_transaction().map_err(storage)?;
    match result {
        Ok(remote) => {
            let n=tx.execute("UPDATE chat_jobs SET state='completed',lease_token=NULL,lease_until=NULL,updated_at=?1 WHERE id=?2 AND lease_token=?3",params![now(),job,lease]).map_err(storage)?;
            if n == 1 {
                tx.execute("UPDATE chat_messages SET state='sent',remote_message_id=?1,delivered_at=?2,error_message=NULL,updated_at=?2 WHERE id=?3",params![remote,now(),message]).map_err(storage)?;
            }
        }
        Err((code, error)) => {
            let permanent = matches!(code, 400 | 403 | 404 | 409);
            let dead = permanent || attempt >= 7;
            let n=tx.execute("UPDATE chat_jobs SET state=?1,available_at=?2,attempt_count=attempt_count+1,last_error=?3,lease_token=NULL,lease_until=NULL,updated_at=?4 WHERE id=?5 AND lease_token=?6",params![if dead {"dead"} else {"queued"},now()+5*(1i64<<attempt.min(6)),error,now(),job,lease]).map_err(storage)?;
            if n == 1 {
                tx.execute("UPDATE chat_messages SET state=?1,error_message=?2,attempt_count=attempt_count+1,next_attempt_at=?3,updated_at=?4 WHERE id=?5",params![if code==403 {"blocked"} else if dead {"failed"} else {"queued"},error,now()+5*(1i64<<attempt.min(6)),now(),message]).map_err(storage)?;
            }
        }
    }
    tx.commit().map_err(storage)
}
