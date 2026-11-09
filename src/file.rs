//! Native resumable files, reachable only through an invocation's private Router.
use std::{
    collections::BTreeMap,
    io::{Read, Seek, SeekFrom},
    sync::Arc,
};

use axum::{
    Router,
    body::Body,
    response::{IntoResponse, Response},
    routing::any,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use cap_std::fs::Dir;
use http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use sha2::Digest;
use tokio::io::AsyncWriteExt;
mod checkpoint;
mod copy;
pub(crate) use copy::library_router;
mod sql;
mod vfs;
use checkpoint::Checkpoint;

type Failure = (u16, String);
const CHUNK: u64 = 4 * 1024 * 1024;
const EXPIRES: i64 = 24 * 3600;
const UPLOAD: &str = "/std/file/upload";
const FILE: &str = "/std/file";
fn fail(code: u16, message: impl Into<String>) -> Failure {
    (code, message.into())
}
fn io(error: std::io::Error) -> Failure {
    fail(503, format!("file storage: {error}"))
}
fn storage(error: rusqlite::Error) -> Failure {
    fail(503, format!("file index: {error}"))
}
fn random_id() -> Result<String, Failure> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|_| fail(503, "randomness unavailable"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
fn valid_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn valid_filename(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && !matches!(value, "." | "..")
        && !value.contains(['/', '\\', '\0'])
        && !value.chars().any(char::is_control)
}
fn collision_name(name: &str, number: u32) -> String {
    if number == 1 {
        return name.to_owned();
    }
    let (stem, extension) = name
        .rsplit_once('.')
        .filter(|(stem, extension)| {
            !stem.is_empty() && !extension.is_empty() && extension.len() <= 32
        })
        .map_or((name, String::new()), |(stem, extension)| {
            (stem, format!(".{extension}"))
        });
    let suffix = format!(" ({number}){extension}");
    let mut end = stem.len().min(255 - suffix.len());
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{suffix}", &stem[..end])
}
fn create_content(dir: &Dir, name: &str) -> Result<String, Failure> {
    for number in 1..=10_000 {
        let candidate = collision_name(name, number);
        let file = rustix::fs::openat(
            dir,
            candidate.as_str(),
            rustix::fs::OFlags::RDWR
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::EXCL
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        );
        match file {
            Ok(file) => {
                std::fs::File::from(file).sync_all().map_err(io)?;
                return Ok(candidate);
            }
            Err(rustix::io::Errno::EXIST) => continue,
            Err(error) => return Err(io(error.into())),
        }
    }
    Err(fail(409, "too many files with this name"))
}
fn valid_directory(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && !value.contains(['\\', '\0', '%'])
        && value
            .split('/')
            .all(|p| !p.is_empty() && p != "." && p != ".." && !p.chars().any(char::is_control))
}
// Every segment is opened no-follow relative to a previously opened directory.
fn directory(root: &Dir, value: &str, create: bool) -> Result<Dir, Failure> {
    if !valid_directory(value) {
        return Err(fail(400, "invalid relative directory"));
    }
    let mut dir = root.try_clone().map_err(io)?;
    for part in value.split('/') {
        if create {
            match dir.create_dir(part) {
                Ok(()) => dir
                    .try_clone()
                    .map_err(io)?
                    .into_std_file()
                    .sync_all()
                    .map_err(io)?,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(io(e)),
            }
        }
        let fd = rustix::fs::openat(
            &dir,
            part,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(|e| io(e.into()))?;
        dir = Dir::from_std_file(std::fs::File::from(fd));
    }
    Ok(dir)
}
fn open(root: &Dir, name: &str, write: bool, create: bool) -> Result<std::fs::File, Failure> {
    let mut flags =
        rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NONBLOCK;
    flags |= if write {
        rustix::fs::OFlags::RDWR
    } else {
        rustix::fs::OFlags::RDONLY
    };
    if create {
        flags |= rustix::fs::OFlags::CREATE | rustix::fs::OFlags::EXCL;
    }
    let fd = rustix::fs::openat(
        root,
        name,
        flags,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )
    .map_err(|e| io(e.into()))?;
    let file = std::fs::File::from(fd);
    if !file.metadata().map_err(io)?.is_file() {
        return Err(fail(503, "invalid file storage object"));
    }
    Ok(file)
}
fn query(request: &Request<Body>, allowed: &[&str]) -> Result<BTreeMap<String, String>, Failure> {
    let q = request.uri().query().unwrap_or("");
    let bytes = q.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b'%'
            && (i + 2 >= bytes.len()
                || !bytes[i + 1].is_ascii_hexdigit()
                || !bytes[i + 2].is_ascii_hexdigit())
        {
            return Err(fail(400, "invalid query encoding"));
        }
    }
    let mut result = BTreeMap::new();
    if q.is_empty() {
        return Ok(result);
    }
    for item in q.split('&') {
        let Some((key, value)) = item.split_once('=') else {
            return Err(fail(400, "invalid query"));
        };
        let decode = |v: &str| {
            percent_encoding::percent_decode_str(&v.replace('+', " "))
                .decode_utf8()
                .map(|v| v.into_owned())
                .map_err(|_| fail(400, "invalid query encoding"))
        };
        let (key, value) = (decode(key)?, decode(value)?);
        if !allowed.contains(&key.as_str())
            || value.is_empty()
            || result.insert(key, value).is_some()
        {
            return Err(fail(400, "invalid or repeated query parameter"));
        }
    }
    Ok(result)
}
fn required<'a>(query: &'a BTreeMap<String, String>, key: &str) -> Result<&'a str, Failure> {
    query
        .get(key)
        .map(String::as_str)
        .ok_or_else(|| fail(400, format!("missing {key}")))
}
fn one<'a>(headers: &'a http::HeaderMap, name: &str) -> Result<Option<&'a str>, Failure> {
    let mut values = headers.get_all(name).iter();
    let value = values.next();
    if values.next().is_some() {
        return Err(fail(400, format!("repeated {name}")));
    }
    value
        .map(|v| v.to_str().map_err(|_| fail(400, format!("invalid {name}"))))
        .transpose()
}
fn number(value: Option<&str>, name: &str) -> Result<u64, Failure> {
    let value = value.ok_or_else(|| fail(400, format!("missing {name}")))?;
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(fail(400, format!("invalid {name}")));
    }
    value
        .parse()
        .map_err(|_| fail(400, format!("invalid {name}")))
}
fn metadata(headers: &http::HeaderMap) -> Result<(String, String), Failure> {
    let mut fields = BTreeMap::new();
    for field in one(headers, "upload-metadata")?
        .unwrap_or("")
        .split(',')
        .filter(|s| !s.is_empty())
    {
        let (key, value) = field
            .trim()
            .split_once(' ')
            .ok_or_else(|| fail(400, "invalid upload metadata"))?;
        let decoded = STANDARD
            .decode(value)
            .map_err(|_| fail(400, "invalid metadata encoding"))?;
        let text = String::from_utf8(decoded).map_err(|_| fail(400, "invalid metadata text"))?;
        if !["filename", "filetype"].contains(&key) || fields.insert(key, text).is_some() {
            return Err(fail(400, "invalid metadata key"));
        }
    }
    let name = fields
        .remove("filename")
        .unwrap_or_else(|| "attachment".into());
    let kind = fields
        .remove("filetype")
        .unwrap_or_else(|| "application/octet-stream".into());
    if !valid_filename(&name)
        || kind.len() > 127
        || !kind.contains('/')
        || !kind.bytes().all(|b| b.is_ascii_graphic())
    {
        return Err(fail(400, "invalid file metadata"));
    }
    Ok((name, kind))
}
#[derive(Clone, PartialEq)]
struct FileRecord {
    id: String,
    request_id: String,
    directory: String,
    name: String,
    media_type: String,
    size: u64,
    offset: u64,
    checkpoint: String,
    digest: Option<String>,
    created_at: i64,
    storage_name: Option<String>,
}

fn record(db: &Connection, id: &str) -> Result<FileRecord, Failure> {
    let row = db
        .query_row(sql::FILE_RECORD, [id], |r| {
            Ok(FileRecord {
                id: r.get(0)?,
                request_id: r.get(1)?,
                directory: r.get(2)?,
                name: r.get(3)?,
                media_type: r.get(4)?,
                size: r.get(5)?,
                offset: r.get(6)?,
                checkpoint: r.get(7)?,
                digest: r.get(8)?,
                created_at: r.get(9)?,
                storage_name: r.get(10)?,
            })
        })
        .optional()
        .map_err(storage)?
        .ok_or_else(|| fail(404, "file not found"))?;
    if !valid_id(row.id.as_str())
        || !(0..=253_402_214_399).contains(&row.created_at)
        || !valid_directory(row.directory.as_str())
        || row
            .storage_name
            .as_deref()
            .is_some_and(|name| !valid_filename(name))
        || row.offset > row.size
    {
        return Err(fail(503, "invalid file index"));
    }
    Ok(row)
}
fn progress(value: &FileRecord) -> Result<(), Failure> {
    if value.digest.is_none() && chrono::Utc::now().timestamp() >= value.created_at + EXPIRES {
        return Err(fail(410, "upload expired"));
    }
    Ok(())
}
fn content_dir(root: &Dir, value: &FileRecord) -> Result<Dir, Failure> {
    let archive = value.directory.as_str();
    let path = if value.storage_name.is_none() {
        // Previously installed native records remain readable until explicitly moved.
        format!("{archive}/{}", value.id.as_str())
    } else {
        archive.to_owned()
    };
    directory(root, &path, false)
}
fn content_name(value: &FileRecord) -> &str {
    value.storage_name.as_deref().unwrap_or("content")
}
fn fact(value: &FileRecord) -> Value {
    let expires = if value.digest.is_none() {
        chrono::DateTime::from_timestamp(value.created_at + EXPIRES, 0).map(|t| t.to_rfc3339())
    } else {
        None
    };
    json!({"id":value.id,"name":value.name,"media_type":value.media_type,"size":value.size,"offset":value.offset,"digest":value.digest,"expires_at":expires})
}
fn upload_headers(response: &mut Response, value: &FileRecord) {
    for (key, value) in [
        ("upload-offset", value.offset.to_string()),
        ("upload-length", value.size.to_string()),
    ] {
        response.headers_mut().insert(
            http::HeaderName::from_bytes(key.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
    }
    if value.digest.is_none() {
        let time = std::time::UNIX_EPOCH
            + std::time::Duration::from_secs((value.created_at + EXPIRES).max(0) as u64);
        response.headers_mut().insert(
            "upload-expires",
            httpdate::fmt_http_date(time).parse().unwrap(),
        );
    }
}
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, Failure> + Send + 'static,
) -> Result<T, Failure> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| fail(503, "file operation interrupted"))?
}
async fn index<T: Send + 'static>(
    root: Arc<Dir>,
    action: impl FnOnce(&mut Connection) -> Result<T, Failure> + Send + 'static,
) -> Result<T, Failure> {
    blocking(move || {
        vfs::with(root, |db| {
            db.execute_batch(include_str!("file/sql/schema.sql"))
                .map_err(storage)?;
            let has_name = |db: &Connection| -> Result<bool, Failure> {
                db.query_row(sql::HAS_STORAGE_NAME, [], |r| r.get(0))
                    .map_err(storage)
            };
            if !has_name(db)? {
                let tx = db
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(storage)?;
                if !has_name(&tx)? {
                    tx.execute_batch(include_str!("file/sql/add_storage_name.sql"))
                        .map_err(storage)?;
                }
                tx.commit().map_err(storage)?;
            }
            action(db)
        })
    })
    .await
}

pub(crate) fn router(root: Dir, library: Router) -> Router {
    let root = Arc::new(root);
    let copies = copy::router(root.clone(), library);
    let handler = move |request: Request<Body>| {
        let root = root.clone();
        async move {
            let upload = request.uri().path() == UPLOAD;
            let result = serve(root, request).await;
            let mut response = match result {
                Ok(response) => response,
                Err((code, message)) => (
                    StatusCode::from_u16(code).unwrap(),
                    axum::Json(json!({"error":message})),
                )
                    .into_response(),
            };
            if !response.headers().contains_key(header::CACHE_CONTROL) {
                response
                    .headers_mut()
                    .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
            }
            response
                .headers_mut()
                .insert("x-content-type-options", "nosniff".parse().unwrap());
            if upload {
                response
                    .headers_mut()
                    .insert("tus-resumable", "1.0.0".parse().unwrap());
                response
                    .headers_mut()
                    .insert("tus-version", "1.0.0".parse().unwrap());
            }
            response
        }
    };
    Router::new()
        .route(FILE, any(handler.clone()))
        .route(UPLOAD, any(handler))
        .merge(copies)
}
async fn serve(root: Arc<Dir>, request: Request<Body>) -> Result<Response, Failure> {
    let upload = request.uri().path() == UPLOAD;
    let method = request.method().clone();
    if upload && method == Method::OPTIONS {
        let q = query(&request, &["id"])?;
        if q.get("id").is_some_and(|id| !valid_id(id)) {
            return Err(fail(400, "invalid id"));
        }
        return Ok((
            StatusCode::NO_CONTENT,
            [
                ("tus-extension", "creation,expiration,termination,checksum"),
                ("tus-checksum-algorithm", "sha1,sha256"),
            ],
        )
            .into_response());
    }
    if upload && one(request.headers(), "tus-resumable")? != Some("1.0.0") {
        return Err(fail(412, "unsupported tus version"));
    }
    let root = Arc::new(blocking(move || directory(&root, ".file", true)).await?);
    if upload && method == Method::POST {
        return create(root, request).await;
    }
    let q = query(
        &request,
        if upload {
            &["id"]
        } else if method == Method::GET {
            &["id", "view"]
        } else {
            &["id"]
        },
    )?;
    let id = required(&q, "id")?.to_owned();
    if !valid_id(&id) {
        return Err(fail(400, "invalid id"));
    }
    if q.get("view").is_some_and(|v| v != "meta") {
        return Err(fail(400, "invalid view"));
    }
    if upload && method == Method::DELETE {
        let cleanup = root.clone();
        index(root, move |db| {
            let tx = db
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage)?;
            let value = record(&tx, &id)?;
            tx.execute(sql::DELETE_FILE, [&id]).map_err(storage)?;
            tx.commit().map_err(storage)?;
            // Index disappearance wins against all later PATCH commits.
            if let Ok(dir) = content_dir(&cleanup, &value) {
                let _ = dir.remove_file(content_name(&value));
                let _ = dir.into_std_file().sync_all();
            }
            Ok(())
        })
        .await?;
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    let value = index(root.clone(), move |db| record(db, &id)).await?;
    progress(&value)?;
    if upload {
        if method == Method::HEAD {
            let mut response = StatusCode::OK.into_response();
            upload_headers(&mut response, &value);
            return Ok(response);
        }
        if method == Method::PATCH {
            return patch(root, request, value).await;
        }
        return Err(fail(405, "method not allowed"));
    }
    if method != Method::GET && method != Method::HEAD {
        return Err(fail(405, "method not allowed"));
    }
    if q.contains_key("view") {
        return Ok(axum::Json(fact(&value)).into_response());
    }
    download(root, request, value).await
}
async fn create(root: Arc<Dir>, mut request: Request<Body>) -> Result<Response, Failure> {
    let q = query(&request, &["request_id", "directory"])?;
    let key = required(&q, "request_id")?.to_owned();
    let location = required(&q, "directory")?.to_owned();
    if key.len() > 128
        || !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
        || !valid_directory(&location)
    {
        return Err(fail(400, "invalid creation parameters"));
    }
    let size = number(one(request.headers(), "upload-length")?, "upload-length")?;
    if size > i64::MAX as u64 {
        return Err(fail(413, "file too large"));
    }
    if request.headers().contains_key("upload-defer-length")
        || request.headers().contains_key("upload-concat")
    {
        return Err(fail(400, "unsupported tus extension"));
    }
    let (name, kind) = metadata(request.headers())?;
    while let Some(frame) = request.body_mut().frame().await {
        let frame = frame.map_err(|_| fail(400, "creation body interrupted"))?;
        if frame.data_ref().is_none_or(|v| !v.is_empty()) {
            return Err(fail(400, "creation with upload is not supported"));
        }
    }
    let files = root.clone();
    let value = index(root, move |db| {
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let existing: Option<String> = tx
            .query_row(sql::CREATION_RESOURCE, [&key], |r| r.get(0))
            .optional()
            .map_err(storage)?;
        if let Some(id) = existing {
            let value = record(&tx, &id)?;
            if value.directory != location
                || value.size != size
                || value.name != name
                || value.media_type != kind
            {
                return Err(fail(409, "creation key has different parameters"));
            }
            progress(&value)?;
            return Ok(value);
        }
        let id = random_id()?;
        let dir = directory(&files, &location, true)?;
        let storage_name = create_content(&dir, &name)?;
        dir.into_std_file().sync_all().map_err(io)?;
        let state = serde_json::to_string(&Checkpoint {
            offset: 0,
            stack: vec![],
        })
        .unwrap();
        let digest = if size == 0 {
            Some(blake3::hash(&[]).to_hex().to_string())
        } else {
            None
        };
        tx.execute(
            sql::CREATE_FILE,
            params![
                id,
                key,
                location,
                name,
                kind,
                size,
                state,
                digest,
                chrono::Utc::now().timestamp(),
                storage_name
            ],
        )
        .map_err(storage)?;
        let value = record(&tx, &id)?;
        tx.commit().map_err(storage)?;
        Ok(value)
    })
    .await?;
    let mut response = StatusCode::CREATED.into_response();
    response.headers_mut().insert(
        header::LOCATION,
        format!("{UPLOAD}?id={}", value.id.as_str())
            .parse()
            .unwrap(),
    );
    upload_headers(&mut response, &value);
    Ok(response)
}
async fn patch(
    root: Arc<Dir>,
    mut request: Request<Body>,
    value: FileRecord,
) -> Result<Response, Failure> {
    if one(request.headers(), "content-type")? != Some("application/offset+octet-stream") {
        return Err(fail(415, "expected application/offset+octet-stream"));
    }
    let offset = number(one(request.headers(), "upload-offset")?, "upload-offset")?;
    if offset != value.offset {
        let mut response = (
            StatusCode::CONFLICT,
            axum::Json(json!({"error":"upload offset conflict"})),
        )
            .into_response();
        upload_headers(&mut response, &value);
        return Ok(response);
    }
    if !value.digest.is_none() {
        return Err(fail(409, "complete files are immutable"));
    }
    let checksum = one(request.headers(), "upload-checksum")?
        .map(|v| {
            let (algorithm, digest) = v
                .split_once(' ')
                .ok_or_else(|| fail(400, "invalid checksum"))?;
            let bytes = STANDARD
                .decode(digest)
                .map_err(|_| fail(400, "invalid checksum encoding"))?;
            if !matches!((algorithm, bytes.len()), ("sha1", 20) | ("sha256", 32)) {
                return Err(fail(400, "unsupported checksum"));
            }
            Ok((algorithm.to_owned(), bytes))
        })
        .transpose()?;
    let length = one(request.headers(), "content-length")?
        .map(|v| number(Some(v), "content-length"))
        .transpose()?;
    let remaining = value.size - offset;
    if length.is_some_and(|n| n > CHUNK || n > remaining) {
        return Err(fail(413, "upload chunk exceeds allowed length"));
    }
    let files = root.clone();
    let snapshot = value.clone();
    let (mut state, mut tail, temp, name) = blocking(move || {
        let state: Checkpoint = serde_json::from_str(snapshot.checkpoint.as_str())
            .map_err(|_| fail(503, "invalid checkpoint"))?;
        if state.offset != offset
            || state.stack.len()
                != ((offset - state.tail_len() as u64) / 1024).count_ones() as usize
        {
            return Err(fail(503, "invalid checkpoint"));
        }
        let dir = content_dir(&files, &snapshot)?;
        let mut content = open(&dir, content_name(&snapshot), false, false)?;
        if content.metadata().map_err(io)?.len() < offset {
            return Err(fail(503, "content shorter than committed offset"));
        }
        let mut tail = vec![0; state.tail_len()];
        content
            .seek(SeekFrom::Start(offset - tail.len() as u64))
            .map_err(io)?;
        content.read_exact(&mut tail).map_err(io)?;
        let temp = directory(&files, "temporary", true)?;
        let name = random_id()?;
        Ok((state, tail, temp, name))
    })
    .await?;
    let temp = Arc::new(temp);
    let cleanup = scopeguard::guard((temp.clone(), name.clone()), |(dir, name)| {
        let _ = dir.remove_file(name);
    });
    let mut candidate = tokio::fs::File::from_std(open(&temp, &name, true, true)?);
    let mut sha1 = sha1::Sha1::new();
    let mut sha256 = sha2::Sha256::new();
    let mut received = 0u64;
    while let Some(frame) = request.body_mut().frame().await {
        let bytes = frame
            .map_err(|_| fail(400, "upload body interrupted"))?
            .into_data()
            .map_err(|_| fail(400, "upload trailers unsupported"))?;
        received = received
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| fail(413, "chunk too large"))?;
        if received > CHUNK || received > remaining {
            return Err(fail(413, "upload chunk exceeds allowed length"));
        }
        sha1.update(&bytes);
        sha256.update(&bytes);
        state.append(&mut tail, &bytes);
        candidate.write_all(&bytes).await.map_err(io)?;
    }
    if length.is_some_and(|n| n != received) {
        return Err(fail(400, "upload body length mismatch"));
    }
    if let Some((algorithm, expected)) = checksum {
        let actual = if algorithm == "sha1" {
            sha1.finalize().to_vec()
        } else {
            sha256.finalize().to_vec()
        };
        if actual != expected {
            return Err(fail(460, "checksum mismatch"));
        }
    }
    candidate.sync_all().await.map_err(io)?;
    drop(candidate);
    let next_checkpoint = serde_json::to_string(&state).unwrap();
    let digest = if state.offset == value.size {
        Some(state.finish(&tail).to_hex().to_string())
    } else {
        None
    };
    let files = root.clone();
    let value = index(root, move |db| {
        // Own cleanup through the short commit even if the HTTP future drops.
        let _cleanup = cleanup;
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let current = record(&tx, value.id.as_str())?;
        progress(&current)?;
        if current != value {
            return Err(fail(409, "upload changed during patch"));
        }
        let dir = content_dir(&files, &current)?;
        let mut content = open(&dir, content_name(&current), true, false)?;
        if content.metadata().map_err(io)?.len() < offset {
            return Err(fail(503, "content shorter than committed offset"));
        }
        content.set_len(offset).map_err(io)?;
        content.seek(SeekFrom::Start(offset)).map_err(io)?;
        let candidate = open(&temp, &name, false, false)?;
        let copied = std::io::copy(&mut candidate.take(received + 1), &mut content).map_err(io)?;
        if copied != received {
            return Err(fail(503, "candidate length changed"));
        }
        content.sync_all().map_err(io)?;
        dir.into_std_file().sync_all().map_err(io)?;
        #[cfg(test)]
        tests::fault(current.id.as_str(), "after_content_sync")?;
        tx.execute(
            sql::COMMIT_OFFSET,
            params![state.offset, next_checkpoint, digest, current.id.as_str()],
        )
        .map_err(storage)?;
        let next = record(&tx, current.id.as_str())?;
        tx.commit().map_err(storage)?;
        #[cfg(test)]
        tests::fault(current.id.as_str(), "after_index_commit")?;
        Ok(next)
    })
    .await?;
    let mut response = StatusCode::NO_CONTENT.into_response();
    upload_headers(&mut response, &value);
    Ok(response)
}
fn etags(value: &str, tag: &str, weak: bool) -> bool {
    value
        .split(',')
        .map(str::trim)
        .any(|v| v == "*" || v == tag || (weak && v.strip_prefix("W/") == Some(tag)))
}
fn range(value: &str, size: u64) -> Result<Option<(u64, u64)>, Failure> {
    let Some(value) = value.strip_prefix("bytes=") else {
        return Ok(None);
    };
    if value.contains(',') {
        return Ok(None);
    }
    let (start, end) = value
        .split_once('-')
        .ok_or_else(|| fail(416, "invalid range"))?;
    if size == 0 {
        return Err(fail(416, "range not satisfiable"));
    }
    if start.is_empty() {
        let n = number(Some(end), "range").map_err(|_| fail(416, "invalid range"))?;
        if n == 0 {
            return Err(fail(416, "invalid range"));
        }
        return Ok(Some((size.saturating_sub(n), size - 1)));
    }
    let start = number(Some(start), "range").map_err(|_| fail(416, "invalid range"))?;
    let end = if end.is_empty() {
        size - 1
    } else {
        number(Some(end), "range")
            .map_err(|_| fail(416, "invalid range"))?
            .min(size - 1)
    };
    if start >= size || end < start {
        return Err(fail(416, "range not satisfiable"));
    }
    Ok(Some((start, end)))
}
async fn download(
    root: Arc<Dir>,
    request: Request<Body>,
    value: FileRecord,
) -> Result<Response, Failure> {
    let digest = value
        .digest
        .as_deref()
        .ok_or_else(|| fail(409, "upload incomplete"))?;
    if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(fail(503, "invalid content digest"));
    }
    let tag = format!("\"blake3-{digest}\"");
    let size = value.size;
    let modified =
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(value.created_at.max(0) as u64);
    let mut status = StatusCode::OK;
    if let Some(condition) = one(request.headers(), "if-match")? {
        if !etags(condition, &tag, false) {
            status = StatusCode::PRECONDITION_FAILED;
        }
    } else if let Some(condition) = one(request.headers(), "if-unmodified-since")?
        && httpdate::parse_http_date(condition).is_ok_and(|t| modified > t)
    {
        status = StatusCode::PRECONDITION_FAILED;
    }
    if status == StatusCode::OK {
        if let Some(condition) = one(request.headers(), "if-none-match")? {
            if etags(condition, &tag, true) {
                status = StatusCode::NOT_MODIFIED;
            }
        } else if let Some(condition) = one(request.headers(), "if-modified-since")?
            && httpdate::parse_http_date(condition).is_ok_and(|t| modified <= t)
        {
            status = StatusCode::NOT_MODIFIED;
        }
    }
    let mut selected = None;
    if status == StatusCode::OK
        && request.method() == Method::GET
        && let Some(spec) = one(request.headers(), "range")?
    {
        let if_range = one(request.headers(), "if-range")?;
        let matches = if_range
            .is_none_or(|v| v == tag || httpdate::parse_http_date(v).is_ok_and(|t| modified <= t));
        if matches {
            match range(spec, size) {
                Ok(r) => selected = r,
                Err(_) => status = StatusCode::RANGE_NOT_SATISFIABLE,
            }
        }
    }
    let (start, len) = selected
        .map(|(start, end)| (start, end - start + 1))
        .unwrap_or((0, size));
    if selected.is_some() {
        status = StatusCode::PARTIAL_CONTENT;
    }
    let media_type = value.media_type.as_str().to_owned();
    let filename = value.name.as_str();
    let disposition = format!(
        "attachment; filename=\"attachment\"; filename*=UTF-8''{}",
        percent_encoding::utf8_percent_encode(filename, percent_encoding::NON_ALPHANUMERIC)
    );
    let body = if request.method() == Method::HEAD
        || !matches!(status, StatusCode::OK | StatusCode::PARTIAL_CONTENT)
    {
        Body::empty()
    } else {
        let file = blocking(move || {
            let mut file = open(
                &content_dir(&root, &value)?,
                content_name(&value),
                false,
                false,
            )?;
            if file.metadata().map_err(io)?.len() != size {
                return Err(fail(503, "complete file length changed"));
            }
            file.seek(SeekFrom::Start(start)).map_err(io)?;
            Ok(file)
        })
        .await?;
        use tokio::io::AsyncReadExt;
        Body::from_stream(tokio_util::io::ReaderStream::with_capacity(
            tokio::fs::File::from_std(file).take(len),
            16 * 1024,
        ))
    };
    // All metadata below is captured before moving the record into the opener.
    let mut response = Response::new(body);
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        media_type
            .parse()
            .map_err(|_| fail(503, "invalid stored media type"))?,
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        disposition
            .parse()
            .map_err(|_| fail(503, "invalid stored filename"))?,
    );
    response
        .headers_mut()
        .insert(header::ETAG, tag.parse().unwrap());
    response.headers_mut().insert(
        header::LAST_MODIFIED,
        httpdate::fmt_http_date(modified).parse().unwrap(),
    );
    response
        .headers_mut()
        .insert(header::ACCEPT_RANGES, "bytes".parse().unwrap());
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        if status.is_client_error() {
            "no-store"
        } else {
            "private, no-cache"
        }
        .parse()
        .unwrap(),
    );
    if status == StatusCode::RANGE_NOT_SATISFIABLE {
        response.headers_mut().insert(
            header::CONTENT_RANGE,
            format!("bytes */{size}").parse().unwrap(),
        );
    }
    if let Some((start, end)) = selected {
        response.headers_mut().insert(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{size}").parse().unwrap(),
        );
    }
    if matches!(
        status,
        StatusCode::OK | StatusCode::PARTIAL_CONTENT | StatusCode::NOT_MODIFIED
    ) {
        response
            .headers_mut()
            .insert(header::CONTENT_LENGTH, len.to_string().parse().unwrap());
    }
    Ok(response)
}

#[cfg(test)]
#[path = "../tests/unit/file/tests.rs"]
mod tests;
