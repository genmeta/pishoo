//! Host-approved copies from a Lib's private directory to the owner's library.
use tower::ServiceExt;

use super::*;

const MAX_IMPORT_BYTES: u64 = 1024 * 1024 * 1024;

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
                    if request.method() == Method::GET {
                        read_library(root, request).await
                    } else {
                        save(root, request).await
                    }
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
            let file = open(
                &directory(&directory(&root, ".file", false)?, parent, false)?,
                leaf,
                false,
                false,
            )?;
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
            dir.open(".").map_err(io)?.sync_all().map_err(io)?;
        }
        dir = next;
    }
    Ok(dir)
}

#[cfg(test)]
#[path = "../../tests/unit/file/copy.rs"]
mod tests;

async fn read_library(root: Dir, request: Request<Body>) -> Result<Response, Failure> {
    let q = query(&request, &["path", "view", "version"])?;
    let path = required(&q, "path")?.to_owned();
    let expected = q.get("version").cloned();
    let info = q.get("view").is_some_and(|value| value == "meta");
    // Imports use metadata to resume each range and request the digest at completion.
    let hash_contents = info && one(request.headers(), "prefer")? != Some("return=minimal");
    let (file,metadata)=blocking(move || {
        if !valid_directory(&path){return Err(fail(400,"invalid personal file path"));}
        let mut parts=path.split('/').collect::<Vec<_>>();
        let name=parts.pop().unwrap().to_owned();
        let mut directory=root;
        for part in parts{
            let fd=rustix::fs::openat(&directory,part,rustix::fs::OFlags::RDONLY|rustix::fs::OFlags::DIRECTORY|rustix::fs::OFlags::NOFOLLOW|rustix::fs::OFlags::CLOEXEC,rustix::fs::Mode::empty()).map_err(|e|io(e.into()))?;
            directory=Dir::from_std_file(std::fs::File::from(fd));
        }
        let mut file=open(&directory,&name,false,false)?;
        let selected = file.metadata().map_err(io)?;
        let size = selected.len();
        if size > MAX_IMPORT_BYTES { return Err(fail(413, "personal file exceeds import limit")); }
        let version = source_version(&selected);
        if expected.as_ref().is_some_and(|value|value!=&version){return Err(fail(409,"personal file version changed"));}
        let mut metadata=json!({"name":name,"size":size,"version":version,"media_type":mime_guess::from_path(&name).first_or_octet_stream().essence_str()});
        if hash_contents {
            let mut digest = blake3::Hasher::new();
            let mut buffer = [0; 64 * 1024];
            loop {
                let count = file.read(&mut buffer).map_err(io)?;
                if count == 0 { break; }
                digest.update(&buffer[..count]);
            }
            if version != source_version(&file.metadata().map_err(io)?) {
                return Err(fail(409, "personal file version changed"));
            }
            metadata["digest"] = digest.finalize().to_hex().to_string().into();
        }
        Ok((file,metadata))
    }).await?;
    if info {
        return Ok(axum::Json(metadata).into_response());
    }
    crate::static_files::response(file, metadata["name"].as_str().unwrap(), &request)
        .map_err(|error| fail(503, error.to_string()))
}

fn source_version(metadata: &std::fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;
    format!(
        "{:x}",
        sha2::Sha256::digest(
            format!(
                "{}:{}:{}:{}:{}:{}:{}",
                metadata.dev(),
                metadata.ino(),
                metadata.len(),
                metadata.mtime(),
                metadata.mtime_nsec(),
                metadata.ctime(),
                metadata.ctime_nsec()
            )
            .as_bytes()
        )
    )
}
pub(crate) fn personal_files_router(root: std::path::PathBuf, endpoint: dhttp::Endpoint) -> Router {
    Router::new().route(
        "/std/sys/file/list",
        any(move |request: Request<Body>| {
            let (root, endpoint) = (root.clone(), endpoint.clone());
            async move { reply(list_library(root, endpoint, request).await) }
        }),
    )
}
async fn list_library(
    root: std::path::PathBuf,
    endpoint: dhttp::Endpoint,
    request: Request<Body>,
) -> Result<Response, Failure> {
    crate::package::require_owner(&endpoint, &request)
        .map_err(|_| fail(403, "owner required for personal files"))?;
    if request.method() != Method::GET {
        return Err(fail(405, "file listing requires GET"));
    }
    let query = query(&request, &["path", "limit", "cursor"])?;
    let path = query.get("path").cloned().unwrap_or_default();
    if !path.is_empty() && !valid_directory(&path) {
        return Err(fail(400, "invalid personal directory"));
    }
    let limit = query
        .get("limit")
        .map(|s| s.parse::<usize>().map_err(|_| fail(400, "invalid limit")))
        .transpose()?
        .unwrap_or(100);
    if !(1..=100).contains(&limit) {
        return Err(fail(400, "invalid limit"));
    }
    let cursor = query.get("cursor").cloned();
    let page=blocking(move||{
        use std::os::unix::fs::MetadataExt;
        let root=match rustix::fs::open(&root,rustix::fs::OFlags::RDONLY|rustix::fs::OFlags::DIRECTORY|rustix::fs::OFlags::NOFOLLOW|rustix::fs::OFlags::CLOEXEC,rustix::fs::Mode::empty()){
            Ok(fd)=>Dir::from_std_file(std::fs::File::from(fd)),
            Err(rustix::io::Errno::NOENT)if path.is_empty()=>return Ok(json!({"path":"","items":[],"next_cursor":null})),
            Err(error)=>return Err(io(error.into())),
        };
        let directory=if path.is_empty(){root}else{directory(&root,&path,false)?};
        let before=source_version(&directory.try_clone().map_err(io)?.into_std_file().metadata().map_err(io)?);
        let mut entries=Vec::new();
        for entry in directory.entries().map_err(io)?{
            let entry=entry.map_err(io)?;let name=entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(".pishoo-copy-"){continue;}
            let kind=entry.file_type().map_err(io)?;
            if !kind.is_dir()&&!kind.is_file(){continue;}
            let file=if kind.is_dir(){
                std::fs::File::from(rustix::fs::openat(&directory,name.as_str(),rustix::fs::OFlags::RDONLY|rustix::fs::OFlags::DIRECTORY|rustix::fs::OFlags::NOFOLLOW|rustix::fs::OFlags::CLOEXEC,rustix::fs::Mode::empty()).map_err(|e|io(e.into()))?)
            }else{open(&directory,&name,false,false)?};
            let metadata=file.metadata().map_err(io)?;
            let relative=if path.is_empty(){name.clone()}else{format!("{path}/{name}")};
            let mut item=json!({"name":name,"path":relative,"kind":if kind.is_dir(){"directory"}else{"file"},"modified_at":metadata.mtime()});
            if kind.is_file(){item["size"]=json!(metadata.len());item["media_type"]=json!(mime_guess::from_path(&name).first_or_octet_stream().essence_str());item["version"]=json!(source_version(&metadata));}
            entries.push(item);
        }
        entries.sort_by(|a,b|(a["kind"].as_str().unwrap(),a["name"].as_str().unwrap()).cmp(&(b["kind"].as_str().unwrap(),b["name"].as_str().unwrap())));
        if before!=source_version(&directory.try_clone().map_err(io)?.into_std_file().metadata().map_err(io)?){return Err(fail(409,"directory_changed"));}
        let baseline=format!("{:x}",sha2::Sha256::digest(serde_json::to_vec(&entries).unwrap()));
        let after=if let Some(cursor)=cursor{
            let data=base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(cursor).map_err(|_|fail(400,"invalid cursor"))?;
            let cursor:Value=serde_json::from_slice(&data).map_err(|_|fail(400,"invalid cursor"))?;
            if cursor["path"]!=path||cursor["baseline"]!=baseline{return Err(fail(409,"directory_changed"));}
            cursor["after"].as_u64().ok_or_else(||fail(400,"invalid cursor"))? as usize
        }else{0};
        if after>entries.len(){return Err(fail(409,"directory_changed"));}
        let end=(after+limit).min(entries.len());
        let next=(end<entries.len()).then(||base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({"path":path,"baseline":baseline,"after":end})).unwrap()));
        Ok(json!({"path":path,"items":&entries[after..end],"next_cursor":next}))
    }).await?;
    Ok(axum::Json(page).into_response())
}
