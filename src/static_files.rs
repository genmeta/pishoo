//! HTTP semantics for an already-open static file. Routing owns file selection
//! and authorization; the response owns only the selected file, never a cache.
use std::{
    fs::File,
    io::{Seek, SeekFrom},
    os::unix::fs::MetadataExt,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{body::Body, response::Response};
use futures::StreamExt;
use http::{HeaderMap, Method, Request, StatusCode, header};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio_util::io::ReaderStream;

use crate::{Error, Result};

pub(crate) fn response(
    mut file: File,
    media_path: &str,
    request: &Request<Body>,
) -> Result<Response> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(Error::RouteNotFound);
    }
    let length = metadata.len();
    let mut digest = Sha256::new();
    for value in [
        metadata.dev(),
        metadata.ino(),
        length,
        metadata.mtime() as u64,
        metadata.mtime_nsec() as u64,
        metadata.ctime() as u64,
        metadata.ctime_nsec() as u64,
    ] {
        digest.update(value.to_le_bytes());
    }
    // Metadata does not prove byte-for-byte identity. Do not advertise a strong
    // validator or use this tag to combine independently downloaded fragments.
    let etag = format!("W/\"{:x}\"", digest.finalize());
    let now = SystemTime::now();
    let modified = metadata.modified().ok().and_then(|time| {
        time.min(now)
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|time| UNIX_EPOCH + Duration::from_secs(time.as_secs()))
    });
    let headers = request.headers();
    let status = if let Some(matched) = entity_matches(headers, header::IF_MATCH, &etag, true)? {
        (!matched).then_some(StatusCode::PRECONDITION_FAILED)
    } else {
        modified
            .zip(date(headers, header::IF_UNMODIFIED_SINCE))
            .filter(|(modified, condition)| modified > condition)
            .map(|_| StatusCode::PRECONDITION_FAILED)
    };
    let status = match status {
        Some(status) => Some(status),
        None => {
            if let Some(matched) = entity_matches(headers, header::IF_NONE_MATCH, &etag, false)? {
                matched.then_some(StatusCode::NOT_MODIFIED)
            } else {
                modified
                    .zip(date(headers, header::IF_MODIFIED_SINCE))
                    .filter(|(modified, condition)| modified <= condition)
                    .map(|_| StatusCode::NOT_MODIFIED)
            }
        }
    };
    let mut builder = Response::builder()
        .header(header::DATE, httpdate::fmt_http_date(now))
        .header(header::ETAG, etag)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(
            header::CACHE_CONTROL,
            if status == Some(StatusCode::PRECONDITION_FAILED) {
                "no-store"
            } else {
                "private, no-cache"
            },
        )
        .header("x-content-type-options", "nosniff");
    if let Some(modified) = modified {
        builder = builder.header(header::LAST_MODIFIED, httpdate::fmt_http_date(modified));
    }
    if let Some(status) = status {
        // Axum supplies Content-Length: 0 for an empty body otherwise. A 304
        // may carry only the corresponding full 200 representation's length.
        builder = builder.status(status).header(
            header::CONTENT_LENGTH,
            if status == StatusCode::NOT_MODIFIED {
                length
            } else {
                0
            },
        );
        return builder.body(Body::empty()).map_err(Error::Http);
    }

    // Neither our weak ETag nor a filesystem timestamp proves a strong
    // If-Range match. A full response safely replaces the client's old copy.
    let range = (request.method() == Method::GET && !headers.contains_key(header::IF_RANGE))
        .then(|| byte_range(headers, length))
        .flatten();
    let (start, count) = match range {
        Some(Ok((start, count))) => {
            builder = builder.status(StatusCode::PARTIAL_CONTENT).header(
                header::CONTENT_RANGE,
                format!("bytes {start}-{}/{length}", start + count - 1),
            );
            (start, count)
        }
        Some(Err(())) => {
            let mut response = builder
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{length}"))
                .header(header::CONTENT_LENGTH, 0)
                .body(Body::empty())
                .map_err(Error::Http)?;
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                http::HeaderValue::from_static("no-store"),
            );
            return Ok(response);
        }
        None => (0, length),
    };
    builder = builder.header(header::CONTENT_LENGTH, count).header(
        header::CONTENT_TYPE,
        mime_guess::from_path(media_path)
            .first_or_octet_stream()
            .as_ref(),
    );
    let body = if request.method() == Method::HEAD || count == 0 {
        Body::empty()
    } else {
        file.seek(SeekFrom::Start(start))?;
        let mut reader =
            ReaderStream::with_capacity(tokio::fs::File::from_std(file).take(count), 16 * 1024);
        Body::from_stream(async_stream::stream! {
            let mut remaining = count;
            while let Some(chunk) = reader.next().await {
                match chunk {
                    Ok(chunk) => {
                        remaining -= chunk.len() as u64;
                        yield Ok(chunk);
                    }
                    Err(error) => {
                        yield Err(error);
                        return;
                    }
                }
            }
            if remaining != 0 {
                yield Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "static file shortened during response"));
            }
        })
    };
    builder.body(body).map_err(Error::Http)
}

fn date(headers: &HeaderMap, name: header::HeaderName) -> Option<SystemTime> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    httpdate::parse_http_date(value.to_str().ok()?).ok()
}

fn entity_matches(
    headers: &HeaderMap,
    name: header::HeaderName,
    etag: &str,
    strong: bool,
) -> Result<Option<bool>> {
    if !headers.contains_key(&name) {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    for value in headers.get_all(name).iter() {
        if !bytes.is_empty() {
            bytes.push(b',');
        }
        bytes.extend_from_slice(value.as_bytes());
    }
    let mut rest = bytes.trim_ascii();
    if rest == b"*" {
        return Ok(Some(true));
    }
    let invalid = || Error::BadRequest("invalid entity-tag condition".into());
    let mut matched = false;
    let mut found = false;
    while !rest.is_empty() {
        // RFC list fields tolerate empty elements, but a wildcard cannot be
        // mixed into an entity-tag list. Commas inside quotes are tag bytes.
        rest = rest.trim_ascii_start();
        if let Some(tail) = rest.strip_prefix(b",") {
            rest = tail;
            continue;
        }
        let weak = rest.starts_with(b"W/");
        if weak {
            rest = &rest[2..];
        }
        let tail = rest.strip_prefix(b"\"").ok_or_else(invalid)?;
        let end = tail
            .iter()
            .position(|byte| *byte == b'"')
            .ok_or_else(invalid)?;
        if tail[..end].iter().any(|byte| *byte < 0x21 || *byte == 0x7f) {
            return Err(invalid());
        }
        let tag = &rest[..end + 2];
        matched |= (!strong || !weak && !etag.starts_with("W/"))
            && tag == etag.strip_prefix("W/").unwrap_or(etag).as_bytes();
        found = true;
        rest = rest[end + 2..].trim_ascii_start();
        if !rest.is_empty() {
            rest = rest.strip_prefix(b",").ok_or_else(invalid)?;
        }
    }
    if !found {
        return Err(invalid());
    }
    Ok(Some(matched))
}

// None ignores an unsupported/invalid range; Err is a valid unsatisfiable
// single range; Ok carries the selected offset and exact byte count.
fn byte_range(headers: &HeaderMap, length: u64) -> Option<std::result::Result<(u64, u64), ()>> {
    if length == 0 {
        return None;
    }
    let mut values = headers.get_all(header::RANGE).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    let (unit, range) = value.trim().split_once('=')?;
    if !unit.eq_ignore_ascii_case("bytes") {
        return None;
    }
    let (first, last) = range.trim().split_once('-')?;
    if first.is_empty() {
        let suffix = decimal(last)?;
        let count = number(suffix).min(length);
        return Some(if count == 0 {
            Err(())
        } else {
            Ok((length - count, count))
        });
    }
    let first = decimal(first)?;
    let last = if last.is_empty() {
        None
    } else {
        Some(decimal(last)?)
    };
    if last.is_some_and(|last| (first.len(), first) > (last.len(), last)) {
        return None;
    }
    let start = number(first);
    if start >= length {
        return Some(Err(()));
    }
    let end = last.map_or(length - 1, |last| number(last).min(length - 1));
    Some(Ok((start, end - start + 1)))
}

fn decimal(value: &str) -> Option<&str> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let value = value.trim_start_matches('0');
    Some(if value.is_empty() { "0" } else { value })
}

fn number(value: &str) -> u64 {
    value.bytes().fold(0u64, |number, byte| {
        number
            .saturating_mul(10)
            .saturating_add(u64::from(byte - b'0'))
    })
}

#[cfg(test)]
#[path = "../tests/unit/static_files.rs"]
mod tests;
