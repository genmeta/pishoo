use std::io::{Read, Write};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use wasip2::{
    clocks::wall_clock,
    http::types::{
        Fields, IncomingBody, IncomingRequest, Method, OutgoingBody, OutgoingResponse,
        ResponseOutparam,
    },
};

type Failure = (u16, &'static str);
type Reply = (u16, Value, Option<String>);

struct Handler;

impl wasip2::exports::http::incoming_handler::Guest for Handler {
    fn handle(request: IncomingRequest, out: ResponseOutparam) {
        let target = request.path_with_query().unwrap_or_else(|| "/".into());
        let path = target.split('?').next().unwrap_or("/");
        if path == "/" && matches!(request.method(), Method::Get) {
            respond(
                out,
                200,
                "text/html; charset=utf-8",
                include_str!("../page.html").as_bytes(),
                None,
            );
            return;
        }
        let result = dispatch(&request, &target);
        let (status, document, etag) = result.unwrap_or_else(|(status, code)| {
            (
                status,
                json!({"error": code, "message": message(code)}),
                None,
            )
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
}

fn respond(
    out: ResponseOutparam,
    status: u16,
    content_type: &str,
    bytes: &[u8],
    etag: Option<&str>,
) {
    let headers = Fields::new();
    headers
        .append("content-type", content_type.as_bytes())
        .unwrap();
    headers.append("cache-control", b"no-store").unwrap();
    headers
        .append("x-content-type-options", b"nosniff")
        .unwrap();
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

fn message(code: &str) -> &'static str {
    match code {
        "invalid_input" => "请检查输入内容或便签地址",
        "empty_note" => "请填写标题或正文",
        "too_large" => "便签内容超过大小限制",
        "unsupported_content_type" => "请求必须使用 application/json",
        "not_found" => "这条便签不存在或已被删除",
        "id_exists" => "这条便签已存在，请确认之前的保存结果",
        "version_required" => "保存或删除需要提供当前版本",
        "version_conflict" => "这条便签已在其他设备修改",
        "storage_corrupt" => "便签数据损坏或版本不受支持",
        "commit_uncertain" => "保存结果未确认，请重新读取便签",
        "method_not_allowed" => "不支持此请求方法",
        "storage_busy" => "便签正在被其他请求使用，请稍后重试",
        _ => "便签存储暂时不可用，请保留输入后重试",
    }
}

fn dispatch(request: &IncomingRequest, target: &str) -> Result<Reply, Failure> {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let pairs: Vec<_> = form_urlencoded::parse(query.as_bytes()).collect();
    let param = |key: &str| -> Result<Option<String>, Failure> {
        let values: Vec<_> = pairs.iter().filter(|(name, _)| name == key).collect();
        if values.len() > 1 {
            return Err((400, "invalid_input"));
        }
        Ok(values.first().map(|(_, value)| value.to_string()))
    };
    let mut root = database()?;
    match (path, request.method()) {
        ("/notes", Method::Get) => {
            let search = param("q")?.unwrap_or_default();
            if search.chars().count() > 200 {
                return Err((400, "invalid_input"));
            }
            let search = search.to_lowercase();
            let mut notes = Vec::new();
            let mut statement = root.prepare("SELECT id, title, body, created_at, updated_at, version, kind FROM notes WHERE kind='note'").map_err(storage)?;
            let rows = statement.query_map([], row_note).map_err(storage)?;
            for row in rows {
                let note = row.map_err(storage)?;
                validate_record(&note)?;
                let title = note["title"].as_str().unwrap();
                let body = note["body"].as_str().unwrap();
                if !search.is_empty()
                    && !title.to_lowercase().contains(&search)
                    && !body.to_lowercase().contains(&search)
                {
                    continue;
                }
                notes.push(json!({"id":note["id"], "title":title, "preview":body.chars().take(120).collect::<String>(), "updated_at":note["updated_at"], "version":note["version"]}));
            }
            notes.sort_by(|a, b| {
                b["updated_at"]
                    .as_u64()
                    .cmp(&a["updated_at"].as_u64())
                    .then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
            });
            Ok((200, json!({"notes":notes}), None))
        }
        ("/notes", Method::Post) => {
            let input = input(request)?;
            let id = input["id"]
                .as_str()
                .filter(|id| valid_id(id))
                .ok_or((400, "invalid_input"))?;
            let (title, body) = content(&input)?;
            if latest(&root, id)?.is_some() {
                return Err((409, "id_exists"));
            }
            let now = now();
            let note = json!({"schema":1,"kind":"note","id":id,"title":title,"body":body,"created_at":now,"updated_at":now,"version":"1"});
            commit(&mut root, id, 1, &note, 409)?;
            Ok((201, public_note(note), Some("\"1\"".into())))
        }
        ("/note", method @ (Method::Get | Method::Put | Method::Delete)) => {
            let id = param("id")?
                .filter(|id| valid_id(id))
                .ok_or((400, "invalid_input"))?;
            let current = latest(&root, &id)?
                .filter(|note| note["kind"] != "deleted")
                .ok_or((404, "not_found"))?;
            let version = current["version"]
                .as_str()
                .unwrap()
                .parse::<u64>()
                .map_err(|_| (500, "storage_corrupt"))?;
            if matches!(method, Method::Get) {
                return Ok((200, public_note(current), Some(format!("\"{version}\""))));
            }
            let headers = request.headers().get("if-match");
            if headers.is_empty() {
                return Err((428, "version_required"));
            }
            if headers.len() != 1 || headers[0] != format!("\"{version}\"").as_bytes() {
                return Err((412, "version_conflict"));
            }
            let next = version.checked_add(1).ok_or((500, "storage_unavailable"))?;
            let note = if matches!(method, Method::Delete) {
                json!({"schema":1,"kind":"deleted","id":id,"created_at":current["created_at"],"updated_at":now(),"version":next.to_string()})
            } else {
                let input = input(request)?;
                let (title, body) = content(&input)?;
                json!({"schema":1,"kind":"note","id":id,"title":title,"body":body,"created_at":current["created_at"],"updated_at":now(),"version":next.to_string()})
            };
            commit(&mut root, &id, next, &note, 412)?;
            if matches!(method, Method::Delete) {
                Ok((204, Value::Null, None))
            } else {
                Ok((200, public_note(note), Some(format!("\"{next}\""))))
            }
        }
        ("/note" | "/notes" | "/", _) => Err((405, "method_not_allowed")),
        _ => Err((404, "not_found")),
    }
}

fn input(request: &IncomingRequest) -> Result<Value, Failure> {
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
        if bytes.len() + count > 128 * 1024 {
            return Err((413, "too_large"));
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    drop(stream);
    drop(IncomingBody::finish(incoming));
    let document: Value = serde_json::from_slice(&bytes).map_err(|_| (400, "invalid_input"))?;
    if !document.is_object() {
        return Err((400, "invalid_input"));
    }
    Ok(document)
}

fn content(document: &Value) -> Result<(&str, &str), Failure> {
    let title = document["title"].as_str().ok_or((400, "invalid_input"))?;
    let body = document["body"].as_str().ok_or((400, "invalid_input"))?;
    if title.chars().count() > 200 || body.len() > 64 * 1024 {
        return Err((413, "too_large"));
    }
    if title.trim().is_empty() && body.trim().is_empty() {
        return Err((400, "empty_note"));
    }
    Ok((title, body))
}

fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn now() -> u64 {
    let time = wall_clock::now();
    time.seconds
        .saturating_mul(1000)
        .saturating_add(u64::from(time.nanoseconds / 1_000_000))
}

fn storage(error: rusqlite::Error) -> Failure {
    if let rusqlite::Error::SqliteFailure(code, _) = error {
        return match code.code {
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked => {
                (503, "storage_busy")
            }
            rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase => {
                (500, "storage_corrupt")
            }
            _ => (500, "storage_unavailable"),
        };
    }
    (500, "storage_corrupt")
}

fn database() -> Result<Connection, Failure> {
    // SQLite's existing WASI unix-dotfile VFS coordinates separate guest instances.
    // Never use unix-none or an in-memory database for persisted notes.
    let mut connection = Connection::open_with_flags_and_vfs(
        "/db/note.db",
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
            | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        "unix-dotfile",
    )
    .map_err(storage)?;
    connection
        .busy_timeout(std::time::Duration::from_secs(2))
        .map_err(storage)?;
    connection
        .pragma_update(None, "synchronous", "FULL")
        .map_err(storage)?;
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(storage)?;
    if version == 0 {
        let tx = connection
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
            tx.execute_batch(
                "CREATE TABLE notes (
                id TEXT PRIMARY KEY NOT NULL,
                title TEXT,
                body TEXT,
                created_at INTEGER NOT NULL CHECK(created_at>=0),
                updated_at INTEGER NOT NULL CHECK(updated_at>=0),
                version INTEGER NOT NULL CHECK(version>0),
                kind TEXT NOT NULL CHECK(kind IN ('note','deleted')),
                CHECK((kind='note' AND title IS NOT NULL AND body IS NOT NULL) OR
                      (kind='deleted' AND title IS NULL AND body IS NULL))
            ); PRAGMA user_version=1;",
            )
            .map_err(storage)?;
        } else if version != 1 {
            return Err((500, "storage_corrupt"));
        }
        tx.commit().map_err(storage)?;
    } else if version != 1 {
        return Err((500, "storage_corrupt"));
    }
    Ok(connection)
}

fn row_note(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let mut note = json!({"schema":1,"id":row.get::<_,String>(0)?,"created_at":row.get::<_,i64>(3)?,"updated_at":row.get::<_,i64>(4)?,"version":row.get::<_,i64>(5)?.to_string(),"kind":row.get::<_,String>(6)?});
    if note["kind"] == "note" {
        note["title"] = json!(row.get::<_, String>(1)?);
        note["body"] = json!(row.get::<_, String>(2)?);
    }
    Ok(note)
}

fn validate_record(note: &Value) -> Result<(), Failure> {
    if !note["id"].as_str().is_some_and(valid_id)
        || note["created_at"].as_u64().is_none()
        || note["updated_at"].as_u64().is_none()
        || !note["version"]
            .as_str()
            .and_then(|v| v.parse::<i64>().ok())
            .is_some_and(|v| v > 0)
    {
        return Err((500, "storage_corrupt"));
    }
    match note["kind"].as_str() {
        Some("note") => {
            content(note).map_err(|_| (500, "storage_corrupt"))?;
        }
        Some("deleted") => (),
        _ => return Err((500, "storage_corrupt")),
    }
    Ok(())
}

fn latest(connection: &Connection, id: &str) -> Result<Option<Value>, Failure> {
    let note = connection
        .query_row(
            "SELECT id,title,body,created_at,updated_at,version,kind FROM notes WHERE id=?1",
            [id],
            row_note,
        )
        .optional()
        .map_err(storage)?;
    if let Some(note) = &note {
        validate_record(note)?;
    }
    Ok(note)
}

fn commit(
    connection: &mut Connection,
    id: &str,
    version: u64,
    note: &Value,
    conflict_status: u16,
) -> Result<(), Failure> {
    let version = i64::try_from(version).map_err(|_| (500, "storage_unavailable"))?;
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(storage)?;
    let changed = if version == 1 {
        tx.execute("INSERT OR IGNORE INTO notes(id,title,body,created_at,updated_at,version,kind) VALUES(?1,?2,?3,?4,?5,1,'note')", params![id,note["title"].as_str(),note["body"].as_str(),note["created_at"].as_i64(),note["updated_at"].as_i64()]).map_err(storage)?
    } else {
        tx.execute("UPDATE notes SET title=?1,body=?2,updated_at=?3,version=?4,kind=?5 WHERE id=?6 AND version=?7 AND kind='note'", params![note["title"].as_str(),note["body"].as_str(),note["updated_at"].as_i64(),version,note["kind"].as_str(),id,version-1]).map_err(storage)?
    };
    if changed != 1 {
        return Err((
            conflict_status,
            if conflict_status == 409 {
                "id_exists"
            } else {
                "version_conflict"
            },
        ));
    }
    tx.commit().map_err(|_| (500, "commit_uncertain"))?;
    Ok(())
}

fn public_note(mut note: Value) -> Value {
    if let Some(object) = note.as_object_mut() {
        object.remove("schema");
        object.remove("kind");
    }
    note
}

wasip2::http::proxy::export!(Handler);
