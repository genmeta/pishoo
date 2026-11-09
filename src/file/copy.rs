//! Host-approved copies from a Lib's private directory to the owner's library.
use tower::ServiceExt;

use super::*;

// This router is present only in the host's internal HTTP router. It is never
// merged into the external service or exposed as a WASI filesystem preopen.
pub(crate) fn library_router(root: std::path::PathBuf, endpoint: dhttp::Endpoint) -> Router {
    Router::new().route(
        "/std/file/library",
        any(move |request: Request<Body>| {
            let (root, endpoint) = (root.clone(), endpoint.clone());
            async move {
                let result = async {
                    crate::package::require_owner(&endpoint, &request)
                        .map_err(|_| fail(403, "owner required for personal copies"))?;
                    let root = blocking(move || {
                        let fd = rustix::fs::open(
                            &root,
                            rustix::fs::OFlags::RDONLY
                                | rustix::fs::OFlags::DIRECTORY
                                | rustix::fs::OFlags::NOFOLLOW
                                | rustix::fs::OFlags::CLOEXEC,
                            rustix::fs::Mode::empty(),
                        )
                        .map_err(|e| io(e.into()))?;
                        Ok(Dir::from_std_file(std::fs::File::from(fd)))
                    })
                    .await?;
                    save(root, request).await
                }
                .await;
                reply(result)
            }
        }),
    )
}

pub(super) fn router(root: Arc<Dir>, library: Router) -> Router {
    Router::new().route(
        "/std/file/copy",
        any(move |request: Request<Body>| {
            let (root, library) = (root.clone(), library.clone());
            async move { reply(copy(root, library, request).await) }
        }),
    )
}

fn reply(result: Result<Response, Failure>) -> Response {
    let mut response = match result {
        Ok(response) => response,
        Err((status, message)) => (
            StatusCode::from_u16(status).unwrap(),
            axum::Json(json!({"error":message})),
        )
            .into_response(),
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}

async fn copy(
    root: Arc<Dir>,
    library: Router,
    request: Request<Body>,
) -> Result<Response, Failure> {
    if request.method() != Method::POST {
        return Err(fail(405, "copy requires POST"));
    }
    let q = query(
        &request,
        &["id", "directory", "source", "name", "size", "digest"],
    )?;
    let destination = required(&q, "directory")?;
    if !valid_directory(destination) {
        return Err(fail(400, "invalid relative directory"));
    }
    let root = Arc::new(blocking(move || directory(&root, ".file", false)).await?);
    let (name, size, digest, body) = if let Some(id) = q.get("id") {
        if !valid_id(id) || q.len() != 2 {
            return Err(fail(400, "invalid file copy parameters"));
        }
        let id = id.clone();
        let value = index(root.clone(), move |db| record(db, &id)).await?;
        let digest = value
            .digest
            .clone()
            .filter(|_| value.offset == value.size)
            .ok_or_else(|| fail(409, "upload incomplete"))?;
        let (name, size) = (value.name.clone(), value.size);
        let response = download(root, Request::new(Body::empty()), value).await?;
        if response.status() != StatusCode::OK {
            return Err(fail(409, "source unavailable"));
        }
        (name, size, digest, response.into_body())
    } else {
        // Business backends may copy a validated historical relative location.
        // No identity root, other volume, absolute path or symlink is accepted.
        let source = required(&q, "source")?.to_owned();
        if !valid_directory(&source) {
            return Err(fail(400, "invalid copy source"));
        }
        let name = required(&q, "name")?.to_owned();
        let size = number(Some(required(&q, "size")?), "size")?;
        let digest = required(&q, "digest")?.to_owned();
        let file = blocking(move || {
            let (parent, leaf) = source
                .rsplit_once('/')
                .ok_or_else(|| fail(400, "invalid copy source"))?;
            let file = open(&directory(&root, parent, false)?, leaf, false, false)?;
            if file.metadata().map_err(io)?.len() != size {
                return Err(fail(409, "source size changed"));
            }
            Ok(file)
        })
        .await?;
        let body = Body::from_stream(tokio_util::io::ReaderStream::with_capacity(
            tokio::fs::File::from_std(file),
            16 * 1024,
        ));
        (name, size, digest, body)
    };
    let query = form_urlencoded::Serializer::new(String::new())
        .extend_pairs([
            ("directory", destination),
            ("name", name.as_str()),
            ("size", size.to_string().as_str()),
            ("digest", digest.as_str()),
        ])
        .finish();
    let mut output = Request::builder()
        .method(Method::POST)
        .uri(format!("/std/file/library?{query}"))
        .body(Body::empty())
        .unwrap();
    *output.extensions_mut() = request.extensions().clone();
    // A normal native read owns the source descriptor and supplies bounded
    // chunks. The library verifies its complete digest before publishing.
    *output.body_mut() = body;
    Ok(library.oneshot(output).await.expect("Router is infallible"))
}

async fn save(root: Dir, mut request: Request<Body>) -> Result<Response, Failure> {
    if request.method() != Method::POST {
        return Err(fail(405, "save requires POST"));
    }
    let q = query(&request, &["directory", "name", "size", "digest"])?;
    let destination = required(&q, "directory")?.to_owned();
    let name = required(&q, "name")?.to_owned();
    let size = number(Some(required(&q, "size")?), "size")?;
    let digest = required(&q, "digest")?.to_owned();
    if !valid_directory(&destination)
        || !valid_filename(&name)
        || digest.len() != 64
        || !digest.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(fail(400, "invalid copy metadata"));
    }
    let owner = root
        .try_clone()
        .map_err(io)?
        .into_std_file()
        .metadata()
        .map_err(io)?;
    let directory_name = destination.clone();
    let dir = Arc::new(blocking(move || personal_directory(&root, &directory_name)).await?);
    let temp = format!(".pishoo-copy-{}", random_id()?);
    let file = open(&dir, &temp, true, true)?;
    // Cancellation removes an unpublished partial copy. No persistent job,
    // permission token or hard link to the chat original is retained.
    let cleanup = scopeguard::guard((dir.clone(), temp.clone()), |(dir, temp)| {
        let _ = dir.remove_file(temp);
    });
    inherit_owner(&file, &owner)?;
    let mut file = tokio::fs::File::from_std(file);
    let mut count = 0u64;
    let mut hash = blake3::Hasher::new();
    while let Some(frame) = request.body_mut().frame().await {
        let data = frame
            .map_err(|_| fail(400, "copy interrupted"))?
            .into_data()
            .map_err(|_| fail(400, "copy trailers unsupported"))?;
        count = count
            .checked_add(data.len() as u64)
            .filter(|n| *n <= size)
            .ok_or_else(|| fail(409, "copy length mismatch"))?;
        hash.update(&data);
        file.write_all(&data).await.map_err(io)?;
    }
    if count != size || hash.finalize().to_hex().as_str() != digest {
        return Err(fail(409, "copy integrity mismatch"));
    }
    file.sync_all().await.map_err(io)?;
    drop(file);
    let stored = blocking(move || {
        for number in 1..=10_000 {
            let candidate = collision_name(&name, number);
            match dir.hard_link(&temp, &dir, &candidate) {
                Ok(()) => {
                    dir.try_clone()
                        .map_err(io)?
                        .into_std_file()
                        .sync_all()
                        .map_err(io)?;
                    return Ok(candidate);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(io(e)),
            }
        }
        Err(fail(409, "too many files with this name"))
    })
    .await?;
    drop(cleanup);
    Ok(
        axum::Json(json!({"path":format!("{destination}/{stored}"),"size":size,"digest":digest}))
            .into_response(),
    )
}

fn inherit_owner(file: &std::fs::File, owner: &std::fs::Metadata) -> Result<(), Failure> {
    use std::os::unix::fs::MetadataExt;
    if rustix::process::geteuid().is_root() {
        rustix::fs::fchown(
            file,
            Some(rustix::fs::Uid::from_raw(owner.uid())),
            Some(rustix::fs::Gid::from_raw(owner.gid())),
        )
        .map_err(|e| io(e.into()))?;
    }
    Ok(())
}

fn personal_directory(root: &Dir, path: &str) -> Result<Dir, Failure> {
    let owner = root
        .try_clone()
        .map_err(io)?
        .into_std_file()
        .metadata()
        .map_err(io)?;
    let mut dir = root.try_clone().map_err(io)?;
    for segment in path.split('/') {
        let created = match dir.create_dir(segment) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
            Err(e) => return Err(io(e)),
        };
        let next = directory(&dir, segment, false)?;
        if created {
            let file = next.try_clone().map_err(io)?.into_std_file();
            inherit_owner(&file, &owner)?;
            rustix::fs::fchmod(
                &file,
                rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR | rustix::fs::Mode::XUSR,
            )
            .map_err(|e| io(e.into()))?;
            file.sync_all().map_err(io)?;
            dir.into_std_file().sync_all().map_err(io)?;
        }
        dir = next;
    }
    Ok(dir)
}

#[cfg(test)]
#[path = "../../tests/unit/file/copy.rs"]
mod tests;
