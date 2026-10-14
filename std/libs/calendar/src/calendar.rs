//! Calendar business Lib. Each invocation directly owns its private SQLite connection.
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use std::io::{Read, Write};
use wasip2::http::types::*;

struct Handler;

impl wasip2::exports::http::incoming_handler::Guest for Handler {
    fn handle(request: IncomingRequest, out: ResponseOutparam) {
        let local = pishoo_sdk::identity::local();
        let caller = pishoo_sdk::identity::caller();
        let result = if caller.as_ref() != Some(&local) {
            Err((
                403,
                "Only the calendar owner can access this resource.".into(),
            ))
        } else if !matches!(request.method(), Method::Get | Method::Head)
            && request
                .headers()
                .get("sec-fetch-site")
                .iter()
                .any(|v| v == b"cross-site")
        {
            Err((403, "Cross-site mutations are not allowed.".into()))
        } else {
            dispatch(&request)
        };
        respond(out, result);
    }
}
wasip2::http::proxy::export!(Handler);

fn storage(error: rusqlite::Error) -> (u16, String) {
    if matches!(&error, rusqlite::Error::SqliteFailure(code, _) if matches!(code.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
    {
        (503, "Calendar storage is busy. Retry later.".into())
    } else {
        // Keep database paths and SQL diagnostics out of API responses.
        (500, "Calendar storage failed.".into())
    }
}

fn database() -> Result<Connection, (u16, String)> {
    let mut db = Connection::open_with_flags_and_vfs(
        "/db/calendar.db",
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
    initialize(&mut db)?;
    Ok(db)
}

fn initialize(db: &mut Connection) -> Result<(), (u16, String)> {
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
            return Err((500, "Unrecognized calendar database.".into()));
        }
        tx.execute_batch(include_str!("calendar.sql"))
            .map_err(storage)?;
        tx.pragma_update(None, "user_version", 1).map_err(storage)?;
    } else if version != 1 {
        return Err((500, "Unsupported calendar database version.".into()));
    }
    // Existing databases must have the actual schema. Never silently recreate missing tables.
    tx.prepare("SELECT id,title,description,location,start_at,end_at,all_day,color,created_at,updated_at FROM events LIMIT 0").map_err(storage)?;
    tx.commit().map_err(storage)
}

fn query(target: &str) -> Result<std::collections::HashMap<String, String>, (u16, String)> {
    let mut result = std::collections::HashMap::new();
    for (key, value) in form_urlencoded::parse(
        target
            .split_once('?')
            .map(|(_, q)| q)
            .unwrap_or("")
            .as_bytes(),
    ) {
        if result
            .insert(key.into_owned(), value.into_owned())
            .is_some()
        {
            return Err((400, "Duplicate query parameter.".into()));
        }
    }
    Ok(result)
}

fn millis(value: Option<&str>) -> Result<i64, (u16, String)> {
    let value = value
        .and_then(|s| s.parse::<i64>().ok())
        .ok_or_else(|| (400, "Invalid timestamp parameter.".into()))?;
    if !(0..253402300800000).contains(&value) {
        return Err((400, "Timestamp is outside the supported range.".into()));
    }
    Ok(value)
}

fn event_id(query: &std::collections::HashMap<String, String>) -> Result<&str, (u16, String)> {
    let id = query
        .get("id")
        .map(String::as_str)
        .ok_or_else(|| (400, "Missing event ID.".into()))?;
    if id.len() != 32
        || !id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err((400, "Invalid event ID.".into()));
    }
    Ok(id)
}

fn input(request: &IncomingRequest, updating: bool) -> Result<Value, (u16, String)> {
    let media = request.headers().get("content-type");
    if !media.first().is_some_and(|v| {
        v.split(|b| *b == b';')
            .next()
            .is_some_and(|m| m.eq_ignore_ascii_case(b"application/json"))
    }) {
        return Err((415, "Content-Type must be application/json.".into()));
    }
    let body = request
        .consume()
        .map_err(|_| (400, "Invalid request body.".into()))?;
    let mut bytes = Vec::new();
    body.stream()
        .map_err(|_| (400, "Invalid request body.".into()))?
        .take(64 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| (400, "Failed to read request body.".into()))?;
    if bytes.len() > 64 * 1024 {
        return Err((413, "Request body exceeds the size limit.".into()));
    }
    let value = serde_json::from_slice(&bytes).map_err(|_| (400, "Invalid JSON.".into()))?;
    validate(value, updating)
}

fn validate(mut value: Value, updating: bool) -> Result<Value, (u16, String)> {
    let fields = value
        .as_object()
        .ok_or_else(|| (400, "Expected a JSON object.".into()))?;
    if fields.keys().any(|key| {
        !matches!(
            key.as_str(),
            "title" | "description" | "location" | "start_at" | "end_at" | "all_day" | "color"
        ) && !(updating && key == "updated_at")
    }) {
        return Err((400, "Unknown event field.".into()));
    }
    for (key, limit) in [("title", 120), ("description", 8000), ("location", 240)] {
        let text = value[key]
            .as_str()
            .ok_or_else(|| (400, format!("Invalid {key}.")))?
            .trim();
        if (key == "title" && text.is_empty())
            || text.chars().count() > limit
            || text.chars().any(|c| {
                c.is_control() && !(key == "description" && matches!(c, '\n' | '\r' | '\t'))
            })
        {
            return Err((400, format!("Invalid {key}.")));
        }
        value[key] = Value::String(text.to_owned());
    }
    let start = value["start_at"]
        .as_i64()
        .filter(|v| (0..253402300800000).contains(v))
        .ok_or_else(|| (400, "Invalid start timestamp.".into()))?;
    let end = value["end_at"]
        .as_i64()
        .filter(|v| (0..253402300800000).contains(v))
        .ok_or_else(|| (400, "Invalid end timestamp.".into()))?;
    let all_day = value["all_day"]
        .as_bool()
        .ok_or_else(|| (400, "Invalid all_day value.".into()))?;
    if end <= start || (all_day && (start % 86400000 != 0 || end % 86400000 != 0)) {
        return Err((
            400,
            "End must be after start. All-day boundaries must be UTC midnight.".into(),
        ));
    }
    if !value["color"]
        .as_str()
        .is_some_and(|c| matches!(c, "blue" | "green" | "yellow" | "purple" | "pink"))
    {
        return Err((400, "Invalid event color.".into()));
    }
    if updating
        && !value["updated_at"]
            .as_i64()
            .is_some_and(|v| (0..253402300800000).contains(&v))
    {
        return Err((400, "A valid updated_at version is required.".into()));
    }
    Ok(value)
}

fn row_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    Ok(json!({
        "id":row.get::<_,String>(0)?, "title":row.get::<_,String>(1)?,
        "description":row.get::<_,String>(2)?, "location":row.get::<_,String>(3)?,
        "start_at":row.get::<_,i64>(4)?, "end_at":row.get::<_,i64>(5)?,
        "all_day":row.get::<_,bool>(6)?, "color":row.get::<_,String>(7)?,
        "created_at":row.get::<_,i64>(8)?, "updated_at":row.get::<_,i64>(9)?,
    }))
}

fn event(db: &Connection, id: &str) -> Result<Value, (u16, String)> {
    db.query_row("SELECT id,title,description,location,start_at,end_at,all_day,color,created_at,updated_at FROM events WHERE id=?1", [id], row_event)
        .optional().map_err(storage)?.ok_or_else(|| (404, "Event not found.".into()))
}

fn events(db: &Connection, from: i64, to: i64) -> Result<Value, (u16, String)> {
    if to <= from || to - from > 370 * 86400000 {
        return Err((
            400,
            "Query range must be positive and no longer than 370 days.".into(),
        ));
    }
    let mut stmt = db.prepare("SELECT id,title,description,location,start_at,end_at,all_day,color,created_at,updated_at FROM events WHERE start_at<?2 AND end_at>?1 ORDER BY start_at,id LIMIT 10001").map_err(storage)?;
    let rows = stmt
        .query_map(params![from, to], row_event)
        .map_err(storage)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(storage)?;
    if rows.len() > 10000 {
        return Err((413, "Too many events. Use a smaller date range.".into()));
    }
    Ok(json!({"events":rows}))
}

fn create(db: &Connection, id: &str, doc: &Value, now: i64) -> Result<Value, (u16, String)> {
    db.execute("INSERT INTO events(id,title,description,location,start_at,end_at,all_day,color,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?9)",
        params![id,doc["title"].as_str().unwrap(),doc["description"].as_str().unwrap(),doc["location"].as_str().unwrap(),doc["start_at"].as_i64().unwrap(),doc["end_at"].as_i64().unwrap(),doc["all_day"].as_bool().unwrap(),doc["color"].as_str().unwrap(),now]).map_err(storage)?;
    event(db, id)
}

fn update(db: &mut Connection, id: &str, doc: &Value, now: i64) -> Result<Value, (u16, String)> {
    let tx = db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(storage)?;
    let previous = event(&tx, id)?;
    if previous["updated_at"] != doc["updated_at"] {
        return Err((409, "Event version conflict. Reload before editing.".into()));
    }
    let timestamp = now.max(previous["updated_at"].as_i64().unwrap() + 1);
    tx.execute("UPDATE events SET title=?2,description=?3,location=?4,start_at=?5,end_at=?6,all_day=?7,color=?8,updated_at=?9 WHERE id=?1 AND updated_at=?10",
        params![id,doc["title"].as_str().unwrap(),doc["description"].as_str().unwrap(),doc["location"].as_str().unwrap(),doc["start_at"].as_i64().unwrap(),doc["end_at"].as_i64().unwrap(),doc["all_day"].as_bool().unwrap(),doc["color"].as_str().unwrap(),timestamp,doc["updated_at"].as_i64().unwrap()]).map_err(storage)?;
    let result = event(&tx, id)?;
    tx.commit().map_err(storage)?;
    Ok(result)
}

fn delete(db: &mut Connection, id: &str, updated_at: i64) -> Result<(), (u16, String)> {
    let tx = db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(storage)?;
    let previous = event(&tx, id)?;
    if previous["updated_at"].as_i64() != Some(updated_at) {
        return Err((
            409,
            "Event version conflict. Reload before deleting.".into(),
        ));
    }
    tx.execute(
        "DELETE FROM events WHERE id=?1 AND updated_at=?2",
        params![id, updated_at],
    )
    .map_err(storage)?;
    tx.commit().map_err(storage)
}

fn dispatch(request: &IncomingRequest) -> Result<(u16, Value), (u16, String)> {
    let target = request.path_with_query().unwrap_or_else(|| "/".into());
    let path = target.split('?').next().unwrap();
    let query = query(&target)?;
    let allowed: &[&str] = match (path, request.method()) {
        ("/calendar/events", Method::Get) => &["from", "to"],
        ("/calendar/events", Method::Post) => &[],
        ("/calendar/event", Method::Get | Method::Put) => &["id"],
        ("/calendar/event", Method::Delete) => &["id", "updated_at"],
        ("/calendar/events" | "/calendar/event", _) => {
            return Err((405, "Method not allowed.".into()));
        }
        _ => return Err((404, "Resource not found.".into())),
    };
    if query.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err((400, "Unknown query parameter.".into()));
    }
    // Parse inputs before opening or initializing storage.
    let doc = if matches!(request.method(), Method::Post | Method::Put) {
        Some(input(request, matches!(request.method(), Method::Put))?)
    } else {
        None
    };
    let range = if path == "/calendar/events" && matches!(request.method(), Method::Get) {
        Some((
            millis(query.get("from").map(String::as_str))?,
            millis(query.get("to").map(String::as_str))?,
        ))
    } else {
        None
    };
    let id = if path == "/calendar/event" {
        Some(event_id(&query)?)
    } else {
        None
    };
    let version = if matches!(request.method(), Method::Delete) {
        Some(millis(query.get("updated_at").map(String::as_str))?)
    } else {
        None
    };
    if range.is_some_and(|(from, to)| to <= from || to - from > 370 * 86400000) {
        return Err((
            400,
            "Query range must be positive and no longer than 370 days.".into(),
        ));
    }
    let mut db = database()?;
    match (path, request.method()) {
        ("/calendar/events", Method::Get) => {
            let (from, to) = range.unwrap();
            Ok((200, events(&db, from, to)?))
        }
        ("/calendar/events", Method::Post) => {
            let id = wasip2::random::random::get_random_bytes(16)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>();
            Ok((201, create(&db, &id, doc.as_ref().unwrap(), now())?))
        }
        ("/calendar/event", Method::Get) => Ok((200, event(&db, id.unwrap())?)),
        ("/calendar/event", Method::Put) => Ok((
            200,
            update(&mut db, id.unwrap(), doc.as_ref().unwrap(), now())?,
        )),
        ("/calendar/event", Method::Delete) => {
            delete(&mut db, id.unwrap(), version.unwrap())?;
            Ok((204, Value::Null))
        }
        _ => unreachable!(),
    }
}

fn now() -> i64 {
    let now = wasip2::clocks::wall_clock::now();
    now.seconds as i64 * 1000 + i64::from(now.nanoseconds / 1_000_000)
}

fn problem(status: u16, detail: &str) -> Value {
    let title = match status {
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    };
    json!({"type":"about:blank", "title":title, "status":status, "detail":detail})
}

fn respond(out: ResponseOutparam, result: Result<(u16, Value), (u16, String)>) {
    let (status, value) =
        result.unwrap_or_else(|(status, message)| (status, problem(status, &message)));
    let bytes = if status == 204 {
        Vec::new()
    } else {
        serde_json::to_vec(&value).unwrap()
    };
    let headers = Fields::new();
    headers
        .append(
            "content-type",
            if status >= 400 {
                b"application/problem+json"
            } else {
                b"application/json"
            },
        )
        .unwrap();
    if status >= 400 {
        headers.append("content-language", b"en").unwrap();
    }
    headers
        .append("cache-control", b"private, no-store")
        .unwrap();
    headers
        .append("x-content-type-options", b"nosniff")
        .unwrap();
    let response = OutgoingResponse::new(headers);
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

#[cfg(test)]
mod tests;
