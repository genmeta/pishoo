use std::io::{Read, Write};

use base64::{Engine, engine::general_purpose::STANDARD};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use wasip2::{
    clocks::wall_clock,
    http::types::{
        Fields, IncomingBody, IncomingRequest, Method, OutgoingBody, OutgoingResponse,
        ResponseOutparam,
    },
};

use pishoo_sdk::request_identity;

type Failure = (u16, &'static str);
type Reply = (u16, Value, Option<String>);

struct Handler;

impl wasip2::exports::http::incoming_handler::Guest for Handler {
    fn handle(request: IncomingRequest, out: ResponseOutparam) {
        // Identity never comes from a name in JSON, query parameters or headers.
        let Some(caller) = request_identity::caller_name() else {
            respond_json(out, Err((403, "identity_required")));
            return;
        };
        let owner = request_identity::caller_is_owner();
        let local = request_identity::local_name();
        let target = request.path_with_query().unwrap_or_else(|| "/".into());
        let path = target.split('?').next().unwrap_or("/");
        if path == "/journal/photo" && matches!(request.method(), Method::Get) {
            let result = (|| {
                let id = parameter(&target, "id")?
                    .filter(|id| valid_id(id))
                    .ok_or((400, "invalid_input"))?;
                let db = database()?;
                let image: Option<Option<Vec<u8>>> = db
                    .query_row(
                        "SELECT photo FROM entries WHERE id=?1 AND kind='entry'",
                        [id],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(storage)?;
                image.flatten().ok_or((404, "not_found"))
            })();
            match result {
                Ok(image) => respond(out, 200, "image/jpeg", &image, None),
                Err(failure) => respond_json(out, Err(failure)),
            }
            return;
        }
        respond_json(out, dispatch(&request, &target, &local, &caller, owner));
    }
}

fn respond_json(out: ResponseOutparam, result: Result<Reply, Failure>) {
    let (status, document, etag) = result.unwrap_or_else(|(status, code)| {
        (status, json!({"error":code,"message":message(code)}), None)
    });
    let bytes = if status == 204 {
        Vec::new()
    } else {
        serde_json::to_vec(&document).unwrap()
    };
    respond(
        out,
        status,
        "application/json; charset=utf-8",
        &bytes,
        etag.as_deref(),
    );
}

fn respond(
    out: ResponseOutparam,
    status: u16,
    content_type: &str,
    bytes: &[u8],
    etag: Option<&str>,
) {
    let headers = Fields::new();
    for (name, value) in [
        ("content-type", content_type),
        ("cache-control", "no-store"),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        ("cross-origin-resource-policy", "same-origin"),
        (
            "content-security-policy",
            "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src 'self' blob: data:; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'; object-src 'none'",
        ),
    ] {
        headers.append(name, value.as_bytes()).unwrap();
    }
    if let Some(etag) = etag {
        headers.append("etag", etag.as_bytes()).unwrap();
    }
    let response = OutgoingResponse::new(headers);
    response.set_status_code(status).unwrap();
    let body = response.body().unwrap();
    ResponseOutparam::set(out, Ok(response));
    let mut stream = body.write().unwrap();
    for chunk in bytes.chunks(8192) {
        if stream.write_all(chunk).is_err() {
            return;
        }
    }
    drop(stream);
    let _ = OutgoingBody::finish(body, None);
}

fn dispatch(
    request: &IncomingRequest,
    target: &str,
    local: &str,
    caller: &str,
    owner: bool,
) -> Result<Reply, Failure> {
    let path = target.split('?').next().unwrap_or("/");
    let method = request.method();
    if path == "/journal/context" && matches!(method, Method::Get) {
        return Ok((
            200,
            json!({"owner_name":local,"caller_name":caller,"is_owner":owner}),
            None,
        ));
    }
    // Unsafe operations require a non-simple JSON request. No CORS permission
    // is offered, so a third-party web page cannot submit a write as the user.
    if !matches!(method, Method::Get | Method::Head) {
        if request.headers().get("x-journal-write") != [b"1".to_vec()] {
            return Err((403, "write_header_required"));
        }
        if request
            .headers()
            .get("sec-fetch-site")
            .iter()
            .any(|value| value == b"cross-site")
        {
            return Err((403, "cross_origin_denied"));
        }
        let origins = request.headers().get("origin");
        if origins.len() > 1
            || origins
                .first()
                .is_some_and(|origin| !same_origin(request, origin, caller))
        {
            return Err((403, "cross_origin_denied"));
        }
        if !(path == "/journal/comments" && matches!(method, Method::Post)) && !owner {
            return Err((403, "owner_required"));
        }
    }
    let mut db = database()?;
    match (path, method) {
        ("/journal/entries", Method::Get) => {
            let offset = parameter(target, "offset")?
                .unwrap_or_else(|| "0".into())
                .parse::<i64>()
                .map_err(|_| (400, "invalid_input"))?;
            if !(0..=100_000).contains(&offset) {
                return Err((400, "invalid_input"));
            }
            let mut stmt = db.prepare("SELECT e.id,e.title,substr(e.body,1,180),e.created_at,e.updated_at,e.version,e.photo IS NOT NULL,(SELECT count(*) FROM comments c WHERE c.entry_id=e.id AND c.kind='comment') FROM entries e WHERE e.kind='entry' ORDER BY e.created_at DESC,e.id DESC LIMIT 21 OFFSET ?1").map_err(storage)?;
            let rows = stmt.query_map([offset], |row| Ok(json!({"id":row.get::<_,String>(0)?,"title":row.get::<_,String>(1)?,"preview":row.get::<_,String>(2)?,"created_at":row.get::<_,i64>(3)?,"updated_at":row.get::<_,i64>(4)?,"version":row.get::<_,i64>(5)?.to_string(),"has_photo":row.get::<_,bool>(6)?,"comment_count":row.get::<_,i64>(7)?}))).map_err(storage)?;
            let mut entries = rows
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(storage)?;
            let more = entries.len() > 20;
            entries.truncate(20);
            Ok((
                200,
                json!({"entries":entries,"next_offset":if more { Some(offset+20) } else { None }}),
                None,
            ))
        }
        ("/journal/entries", Method::Post) => {
            let doc = input(request, &["id", "title", "body", "photo"])?;
            let id = document_id(&doc, "id")?;
            let (title, body) = content(&doc)?;
            let photo = photo(&doc)?;
            let now = now();
            let tx = db
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage)?;
            let inserted = tx.execute("INSERT OR IGNORE INTO entries(id,title,body,photo,created_at,updated_at,version,kind) VALUES(?1,?2,?3,?4,?5,?5,1,'entry')", params![id,title,body,photo,now]).map_err(storage)?;
            if inserted != 1 {
                return Err((409, "id_exists"));
            }
            let entry = entry(&tx, id)?;
            tx.commit().map_err(|_| (500, "commit_uncertain"))?;
            Ok((201, entry, Some("\"1\"".into())))
        }
        ("/journal/entry", method @ (Method::Get | Method::Put | Method::Delete)) => {
            let id = parameter(target, "id")?
                .filter(|id| valid_id(id))
                .ok_or((400, "invalid_input"))?;
            if matches!(method, Method::Get) {
                let entry = entry(&db, &id)?;
                let etag = format!("\"{}\"", entry["version"].as_str().unwrap());
                return Ok((200, entry, Some(etag)));
            }
            let version = version(request)?;
            let doc = if matches!(method, Method::Put) {
                Some(input(request, &["title", "body", "photo"])?)
            } else {
                None
            };
            // Input and image processing happen before acquiring the SQLite lock.
            let replacement_photo = match &doc {
                Some(doc) if doc.get("photo").is_some() => Some(photo(doc)?),
                _ => None,
            };
            let tx = db
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage)?;
            let current = entry(&tx, &id)?;
            if current["version"].as_str() != Some(&version.to_string()) {
                return Err((412, "version_conflict"));
            }
            let next = version.checked_add(1).ok_or((500, "storage_unavailable"))?;
            let changed = if let Some(doc) = &doc {
                let (title, body) = content(doc)?;
                if let Some(photo) = &replacement_photo {
                    tx.execute("UPDATE entries SET title=?1,body=?2,photo=?3,updated_at=?4,version=?5 WHERE id=?6 AND version=?7 AND kind='entry'", params![title,body,photo,now(),next,id,version]).map_err(storage)?
                } else {
                    tx.execute("UPDATE entries SET title=?1,body=?2,updated_at=?3,version=?4 WHERE id=?5 AND version=?6 AND kind='entry'", params![title,body,now(),next,id,version]).map_err(storage)?
                }
            } else {
                tx.execute("DELETE FROM comments WHERE entry_id=?1", [&id])
                    .map_err(storage)?;
                tx.execute("UPDATE entries SET title=NULL,body=NULL,photo=NULL,updated_at=?1,version=?2,kind='deleted' WHERE id=?3 AND version=?4 AND kind='entry'", params![now(),next,id,version]).map_err(storage)?
            };
            if changed != 1 {
                return Err((412, "version_conflict"));
            }
            let result = if doc.is_some() {
                (200, entry(&tx, &id)?, Some(format!("\"{next}\"")))
            } else {
                (204, Value::Null, None)
            };
            tx.commit().map_err(|_| (500, "commit_uncertain"))?;
            Ok(result)
        }
        ("/journal/comments", Method::Get) => {
            let id = parameter(target, "entry_id")?
                .filter(|id| valid_id(id))
                .ok_or((400, "invalid_input"))?;
            entry(&db, &id)?;
            let mut stmt = db.prepare("SELECT id,parent_id,author,body,created_at,kind FROM comments WHERE entry_id=?1 ORDER BY created_at,id LIMIT 501").map_err(storage)?;
            let rows = stmt.query_map([id], |row| Ok(json!({"id":row.get::<_,String>(0)?,"parent_id":row.get::<_,Option<String>>(1)?,"author":row.get::<_,Option<String>>(2)?,"body":row.get::<_,Option<String>>(3)?,"created_at":row.get::<_,i64>(4)?,"deleted":row.get::<_,String>(5)?=="deleted"}))).map_err(storage)?;
            let comments = rows
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(storage)?;
            if comments.len() > 500 {
                return Err((500, "storage_corrupt"));
            }
            Ok((200, json!({"comments":comments}), None))
        }
        ("/journal/comments", Method::Post) => {
            let doc = input(request, &["id", "entry_id", "parent_id", "body"])?;
            let id = document_id(&doc, "id")?;
            let entry_id = document_id(&doc, "entry_id")?;
            let body = doc["body"]
                .as_str()
                .filter(|body| !body.trim().is_empty())
                .ok_or((400, "invalid_input"))?;
            if body.len() > 8 * 1024 {
                return Err((413, "too_large"));
            }
            let parent = match doc.get("parent_id") {
                None | Some(Value::Null) => None,
                Some(value) => Some(
                    value
                        .as_str()
                        .filter(|id| valid_id(id))
                        .ok_or((400, "invalid_input"))?,
                ),
            };
            let tx = db
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage)?;
            entry(&tx, entry_id)?;
            if let Some(parent) = parent {
                let root: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM comments WHERE id=?1 AND entry_id=?2 AND parent_id IS NULL AND kind='comment')", params![parent,entry_id], |row| row.get(0)).map_err(storage)?;
                if !root {
                    return Err((400, "invalid_parent"));
                }
            }
            let count: i64 = tx
                .query_row(
                    "SELECT count(*) FROM comments WHERE entry_id=?1",
                    [entry_id],
                    |row| row.get(0),
                )
                .map_err(storage)?;
            if count >= 500 {
                return Err((409, "comments_full"));
            }
            let created_at = now();
            // Caller identity is supplied by WIT, never accepted in the document.
            let changed = tx.execute("INSERT OR IGNORE INTO comments(id,entry_id,parent_id,author,body,created_at,kind) VALUES(?1,?2,?3,?4,?5,?6,'comment')", params![id,entry_id,parent,caller,body,created_at]).map_err(storage)?;
            if changed != 1 {
                return Err((409, "id_exists"));
            }
            tx.commit().map_err(|_| (500, "commit_uncertain"))?;
            Ok((
                201,
                json!({"id":id,"entry_id":entry_id,"parent_id":parent,"author":caller,"body":body,"created_at":created_at,"deleted":false}),
                None,
            ))
        }
        ("/journal/comment", Method::Delete) => {
            let id = parameter(target, "id")?
                .filter(|id| valid_id(id))
                .ok_or((400, "invalid_input"))?;
            let tx = db
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage)?;
            let changed = tx.execute("UPDATE comments SET author=NULL,body=NULL,kind='deleted' WHERE id=?1 AND kind='comment'", [id]).map_err(storage)?;
            if changed != 1 {
                return Err((404, "not_found"));
            }
            tx.commit().map_err(|_| (500, "commit_uncertain"))?;
            Ok((204, Value::Null, None))
        }
        (
            "/journal/context" | "/journal/entries" | "/journal/entry" | "/journal/comments"
            | "/journal/comment" | "/journal/photo",
            _,
        ) => Err((405, "method_not_allowed")),
        _ => Err((404, "not_found")),
    }
}

fn same_origin(request: &IncomingRequest, value: &[u8], caller: &str) -> bool {
    let Ok(text) = std::str::from_utf8(value) else {
        return false;
    };
    let Ok(origin) = text.parse::<http::Uri>() else {
        return false;
    };
    let Some(origin_authority) = origin.authority() else {
        return false;
    };
    // Origin is scheme + authority only, never a URL with user info, path,
    // query or fragment. Do not treat a loosely parsed URL as a valid origin.
    let Some((_, serialized_authority)) = text.split_once("://") else {
        return false;
    };
    if origin_authority.as_str() != serialized_authority || serialized_authority.contains('@') {
        return false;
    }
    let scheme = match request.scheme() {
        Some(wasip2::http::types::Scheme::Http) => "http",
        Some(wasip2::http::types::Scheme::Https) => "https",
        _ => return false,
    };
    if origin.scheme_str() != Some(scheme) {
        return false;
    }
    let Some(target) = request.authority() else {
        return false;
    };
    let Ok(target) = target.parse::<http::uri::Authority>() else {
        return false;
    };
    // dhttp expands the request URI before handing it to WASI, while a
    // browser's Origin still contains the URL's explicit DHTTP shorthand.
    // Bare ~ denotes the authenticated caller's own endpoint, not any server.
    let host = if origin_authority.host() == "~" {
        caller.to_ascii_lowercase()
    } else {
        origin_authority
            .host()
            .replace('~', ".dhttp.net")
            .to_ascii_lowercase()
    };
    if host != target.host().to_ascii_lowercase() {
        return false;
    }
    let default_port = if scheme == "https" { 443 } else { 80 };
    // Authority::port() also returns None for an out-of-range port. Inspect
    // the raw suffix so malformed ports cannot become an implicit default.
    let effective_port = |authority: &http::uri::Authority| -> Option<u16> {
        let suffix = authority
            .as_str()
            .rsplit('@')
            .next()?
            .strip_prefix(authority.host())?;
        if suffix.is_empty() {
            return Some(default_port);
        }
        let number = suffix.strip_prefix(':')?;
        if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        number.parse().ok()
    };
    match (effective_port(origin_authority), effective_port(&target)) {
        (Some(origin), Some(target)) => origin == target,
        _ => false,
    }
}

fn parameter(target: &str, key: &str) -> Result<Option<String>, Failure> {
    let mut value = None;
    for (name, next) in form_urlencoded::parse(
        target
            .split_once('?')
            .map_or("", |(_, query)| query)
            .as_bytes(),
    ) {
        if name == key {
            if value.is_some() {
                return Err((400, "invalid_input"));
            }
            value = Some(next.into_owned());
        }
    }
    Ok(value)
}

fn input(request: &IncomingRequest, allowed: &[&str]) -> Result<Value, Failure> {
    let headers = request.headers().get("content-type");
    if headers.len() != 1
        || !String::from_utf8_lossy(&headers[0])
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .eq_ignore_ascii_case("application/json")
    {
        return Err((415, "unsupported_content_type"));
    }
    let incoming = request.consume().map_err(|_| (400, "invalid_input"))?;
    let mut stream = incoming.stream().map_err(|_| (400, "invalid_input"))?;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let count = Read::read(&mut stream, &mut chunk).map_err(|_| (400, "invalid_input"))?;
        if count == 0 {
            break;
        }
        if bytes.len() + count > 1536 * 1024 {
            return Err((413, "too_large"));
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    drop(stream);
    drop(IncomingBody::finish(incoming));
    let doc: Value = serde_json::from_slice(&bytes).map_err(|_| (400, "invalid_input"))?;
    let object = doc.as_object().ok_or((400, "invalid_input"))?;
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err((400, "invalid_input"));
    }
    Ok(doc)
}

fn document_id<'a>(doc: &'a Value, key: &str) -> Result<&'a str, Failure> {
    doc[key]
        .as_str()
        .filter(|id| valid_id(id))
        .ok_or((400, "invalid_input"))
}
fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn content(doc: &Value) -> Result<(&str, &str), Failure> {
    let title = doc["title"].as_str().ok_or((400, "invalid_input"))?;
    let body = doc["body"]
        .as_str()
        .filter(|body| !body.trim().is_empty())
        .ok_or((400, "empty_entry"))?;
    if title.chars().count() > 200 || body.len() > 64 * 1024 {
        return Err((413, "too_large"));
    }
    Ok((title, body))
}
fn version(request: &IncomingRequest) -> Result<i64, Failure> {
    let values = request.headers().get("if-match");
    if values.is_empty() {
        return Err((428, "version_required"));
    }
    if values.len() != 1 {
        return Err((400, "invalid_input"));
    }
    let value = std::str::from_utf8(&values[0]).map_err(|_| (400, "invalid_input"))?;
    let inner = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .ok_or((400, "invalid_input"))?;
    let version = inner
        .parse::<i64>()
        .ok()
        .filter(|v| *v > 0)
        .ok_or((400, "invalid_input"))?;
    if inner != version.to_string() {
        return Err((400, "invalid_input"));
    }
    Ok(version)
}

fn photo(doc: &Value) -> Result<Option<Vec<u8>>, Failure> {
    match doc.get("photo") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let encoded = value
                .as_str()
                .and_then(|s| s.strip_prefix("data:image/jpeg;base64,"))
                .ok_or((400, "invalid_photo"))?;
            let bytes = STANDARD
                .decode(encoded)
                .map_err(|_| (400, "invalid_photo"))?;
            if bytes.len() > 1024 * 1024 {
                return Err((413, "too_large"));
            }
            let size = imagesize::blob_size(&bytes).map_err(|_| (400, "invalid_photo"))?;
            if size.width == 0 || size.height == 0 || size.width > 2048 || size.height > 2048 {
                return Err((400, "invalid_photo"));
            }
            Ok(Some(strip_jpeg_metadata(&bytes)?))
        }
    }
}

// Retain the JPEG coding segments and entropy only. APPn and COM may hold
// GPS, EXIF, XMP, thumbnails or free-form text and are never published.
fn strip_jpeg_metadata(bytes: &[u8]) -> Result<Vec<u8>, Failure> {
    if !bytes.starts_with(&[0xff, 0xd8]) {
        return Err((400, "invalid_photo"));
    }
    let mut output = vec![0xff, 0xd8];
    let mut position = 2;
    let mut scan_seen = false;
    while position < bytes.len() {
        let start = position;
        if bytes[position] != 0xff {
            return Err((400, "invalid_photo"));
        }
        while position < bytes.len() && bytes[position] == 0xff {
            position += 1;
        }
        let marker = *bytes.get(position).ok_or((400, "invalid_photo"))?;
        position += 1;
        if marker == 0xd9 {
            if !scan_seen || position != bytes.len() {
                return Err((400, "invalid_photo"));
            }
            output.extend_from_slice(&[0xff, 0xd9]);
            return Ok(output);
        }
        if marker == 0 || marker == 0xd8 || (0xd0..=0xd7).contains(&marker) || marker == 1 {
            return Err((400, "invalid_photo"));
        }
        let length_bytes = bytes
            .get(position..position + 2)
            .ok_or((400, "invalid_photo"))?;
        let length = usize::from(u16::from_be_bytes([length_bytes[0], length_bytes[1]]));
        if length < 2 {
            return Err((400, "invalid_photo"));
        }
        let end = position
            .checked_add(length)
            .filter(|end| *end <= bytes.len())
            .ok_or((400, "invalid_photo"))?;
        if !(0xe0..=0xef).contains(&marker) && marker != 0xfe {
            output.extend_from_slice(&bytes[start..end]);
        }
        position = end;
        if marker == 0xda {
            scan_seen = true;
            let scan_start = position;
            while position < bytes.len() {
                if bytes[position] != 0xff {
                    position += 1;
                    continue;
                }
                let mut next = position + 1;
                while next < bytes.len() && bytes[next] == 0xff {
                    next += 1;
                }
                let marker = *bytes.get(next).ok_or((400, "invalid_photo"))?;
                if marker == 0 || (0xd0..=0xd7).contains(&marker) {
                    position = next + 1;
                    continue;
                }
                break;
            }
            output.extend_from_slice(&bytes[scan_start..position]);
        }
    }
    Err((400, "invalid_photo"))
}

fn now() -> i64 {
    let time = wall_clock::now();
    i64::try_from(
        time.seconds
            .saturating_mul(1000)
            .saturating_add(u64::from(time.nanoseconds / 1_000_000)),
    )
    .unwrap_or(i64::MAX)
}

fn database() -> Result<Connection, Failure> {
    let mut db = Connection::open_with_flags_and_vfs(
        "/db/journal.db",
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
                return Err((500, "storage_corrupt"));
            }
            tx.execute_batch("CREATE TABLE entries(
                id TEXT PRIMARY KEY NOT NULL,title TEXT,body TEXT,photo BLOB,
                created_at INTEGER NOT NULL CHECK(created_at>=0),updated_at INTEGER NOT NULL CHECK(updated_at>=0),
                version INTEGER NOT NULL CHECK(version>0),kind TEXT NOT NULL CHECK(kind IN ('entry','deleted')),
                CHECK((kind='entry' AND title IS NOT NULL AND body IS NOT NULL) OR (kind='deleted' AND title IS NULL AND body IS NULL AND photo IS NULL)));
                CREATE TABLE comments(
                id TEXT PRIMARY KEY NOT NULL,entry_id TEXT NOT NULL REFERENCES entries(id),parent_id TEXT REFERENCES comments(id),author TEXT,body TEXT,
                created_at INTEGER NOT NULL CHECK(created_at>=0),kind TEXT NOT NULL CHECK(kind IN ('comment','deleted')),
                CHECK((kind='comment' AND author IS NOT NULL AND body IS NOT NULL) OR (kind='deleted' AND author IS NULL AND body IS NULL)));
                CREATE INDEX entries_date ON entries(created_at DESC,id DESC);
                CREATE INDEX comments_entry ON comments(entry_id,created_at,id);
                PRAGMA user_version=1;").map_err(storage)?;
        } else if version != 1 {
            return Err((500, "storage_corrupt"));
        }
        tx.commit().map_err(storage)?;
    } else if version != 1 {
        return Err((500, "storage_corrupt"));
    }
    Ok(db)
}

fn entry(db: &Connection, id: &str) -> Result<Value, Failure> {
    db.query_row("SELECT id,title,body,created_at,updated_at,version,photo IS NOT NULL FROM entries WHERE id=?1 AND kind='entry'", [id], |row| Ok(json!({"id":row.get::<_,String>(0)?,"title":row.get::<_,String>(1)?,"body":row.get::<_,String>(2)?,"created_at":row.get::<_,i64>(3)?,"updated_at":row.get::<_,i64>(4)?,"version":row.get::<_,i64>(5)?.to_string(),"has_photo":row.get::<_,bool>(6)?}))).optional().map_err(storage)?.ok_or((404,"not_found"))
}

fn storage(error: rusqlite::Error) -> Failure {
    match error {
        rusqlite::Error::SqliteFailure(code, _) => match code.code {
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked => {
                (503, "storage_busy")
            }
            rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase => {
                (500, "storage_corrupt")
            }
            _ => (500, "storage_unavailable"),
        },
        _ => (500, "storage_corrupt"),
    }
}
fn message(code: &str) -> &'static str {
    match code {
        "identity_required" => "请使用已验证的具名身份访问",
        "owner_required" => "只有主页所属身份可以执行此操作",
        "write_header_required" | "cross_origin_denied" => "写入请求来源未通过检查",
        "invalid_input" => "请检查输入内容",
        "empty_entry" => "请写一点正文",
        "invalid_photo" => "请选择有效照片，最长边不超过 2048 像素",
        "invalid_parent" => "回复目标不存在或不属于这篇手记",
        "comments_full" => "这篇手记的留言已达到上限",
        "too_large" => "内容超过大小限制",
        "unsupported_content_type" => "请求需要使用 JSON",
        "not_found" => "内容不存在或已被删除",
        "id_exists" => "编号已经使用，请核对之前的保存结果",
        "version_required" => "修改需要提供当前版本",
        "version_conflict" => "内容已在其他设备修改，请保留草稿后重新读取",
        "commit_uncertain" => "保存结果未确认，请保留草稿并重新读取",
        "storage_busy" => "存储正在被其他请求使用，请稍后重试",
        "storage_corrupt" => "数据库损坏或版本不受支持",
        "method_not_allowed" => "不支持此请求方法",
        _ => "存储暂时不可用，请保留输入后重试",
    }
}

wasip2::http::proxy::export!(Handler);
