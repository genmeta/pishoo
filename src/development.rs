//! Owner-only development routes backed directly by local files.
use std::{
    fs::{File, Metadata},
    io::Read,
    os::unix::fs::MetadataExt,
    sync::{Arc, RwLock},
};

use axum::{Router, body::Body, response::IntoResponse, routing::any};
use dhttp_home::identity::IdentityProfile;
use http::Request;
use http_body_util::BodyExt;
use tower::ServiceExt;

use crate::{
    Error, Result, package,
    sandbox::{Invocation, Lib, Outgoing, Sandbox},
};

pub(crate) fn router(
    profile: IdentityProfile,
    sandbox: Arc<RwLock<Sandbox>>,
    endpoint: dhttp::Endpoint,
    access: Arc<access_control::AccessService>,
    native_api: Router,
    fallback: Router,
) -> Router {
    let route = any(move |request: Request<Body>| {
        let (profile, sandbox, endpoint, access, native_api, fallback) = (
            profile.clone(),
            sandbox.clone(),
            endpoint.clone(),
            access.clone(),
            native_api.clone(),
            fallback.clone(),
        );
        async move {
            let result: Result<axum::response::Response> = async {
                let path = request.uri().path();
                let (frontend, standard, tail) = if let Some(tail) = path.strip_prefix("/app/") {
                    (true, false, tail)
                } else if let Some(tail) = path.strip_prefix("/std/") {
                    (false, true, tail)
                } else {
                    (false, false, path.trim_start_matches('/'))
                };
                let (name, suffix) = tail
                    .split_once('/')
                    .map_or((tail, ""), |(name, _)| (name, &tail[name.len()..]));
                // Unsigned local files are only executable on a separate dev identity.
                // Native services retain precedence over local projects.
                let local_exists = package::valid_id(name).is_ok()
                    && profile.join("dev").join(name).try_exists()?;
                if !local_exists && endpoint.name().starts_with("dev.") {
                    sandbox
                        .write()
                        .unwrap()
                        .dev_libs
                        .retain(|id, _| id.rsplit('/').next() != Some(name));
                }
                if !endpoint.name().starts_with("dev.")
                    || package::valid_id(name).is_err()
                    || (!frontend
                        && (name == "app"
                            || standard
                                && matches!(name, "sys" | "acl" | "contact" | "dhttp" | "file")))
                    || !local_exists
                {
                    return Ok(fallback
                        .oneshot(request)
                        .await
                        .expect("Router is infallible"));
                }
                let name = name.to_owned();
                let suffix = if standard {
                    format!("/{tail}")
                } else {
                    suffix.to_owned()
                };
                let author = author(&profile, &name)?;
                if !frontend && (author == "std.dhttp.net") != standard {
                    return Ok(fallback
                        .oneshot(request)
                        .await
                        .expect("Router is infallible"));
                }
                package::require_owner(&endpoint, &request)?;
                if frontend {
                    return tokio::task::spawn_blocking(move || {
                        let root = root(&profile)?;
                        let directory = child(&root, &name, true)?;
                        let app = child(&directory, "app", true)?;
                        crate::sandbox::app_response(&app, &suffix, &request)
                    })
                    .await?;
                }
                let (lib, tasks) = tokio::task::spawn_blocking(move || {
                    let lib = load(&profile, &sandbox, &author, &name)?;
                    let tasks = sandbox.read().unwrap().tasks.clone();
                    Ok::<_, Error>((lib, tasks))
                })
                .await??;
                let handshake = request
                    .extensions()
                    .get::<dhttp::HandshakeSummary>()
                    .ok_or(Error::MissingHandshake)?;
                let invocation = Invocation::new(
                    lib,
                    endpoint.clone(),
                    handshake,
                    tasks,
                    Outgoing::Granted {
                        endpoint,
                        access,
                        api: native_api,
                    },
                )?;
                let mut request = request.map(|body| body.map_err(Into::into).boxed_unsync());
                let path = if suffix.is_empty() { "/" } else { &suffix };
                let mut parts = request.uri().clone().into_parts();
                parts.path_and_query = Some(
                    match request.uri().query() {
                        Some(query) => format!("{path}?{query}"),
                        None => path.to_owned(),
                    }
                    .parse()
                    .map_err(|_| Error::BadRequest("invalid development path".into()))?,
                );
                *request.uri_mut() = http::Uri::from_parts(parts)
                    .map_err(|_| Error::BadRequest("invalid development URI".into()))?;
                invocation
                    .execute(request)
                    .await
                    .map(|response| response.map(Body::new))
            }
            .await;
            result.unwrap_or_else(|error| {
                let status = error.status();
                if status.is_server_error() {
                    tracing::error!(%error, "development request failed");
                }
                (
                    status,
                    status.canonical_reason().unwrap_or("request failed"),
                )
                    .into_response()
            })
        }
    });
    Router::new().fallback(route)
}

// A shared lock protects file selection from Jade's atomic directory replacement.
// The file itself is the only lifecycle signal; no process/session registration exists.
fn root(profile: &IdentityProfile) -> Result<File> {
    let owner = profile.path().metadata()?;
    let root = File::from(
        rustix::fs::open(
            profile.join("dev"),
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(|_| Error::RouteNotFound)?,
    );
    let metadata = root.metadata()?;
    if metadata.uid() != owner.uid() || metadata.mode() & 0o022 != 0 {
        return Err(Error::Denied);
    }
    rustix::fs::flock(&root, rustix::fs::FlockOperation::LockShared)
        .map_err(std::io::Error::from)?;
    Ok(root)
}

fn child(parent: &File, name: &str, directory: bool) -> Result<File> {
    let mut flags = rustix::fs::OFlags::RDONLY
        | rustix::fs::OFlags::NOFOLLOW
        | rustix::fs::OFlags::CLOEXEC
        | rustix::fs::OFlags::NONBLOCK;
    if directory {
        flags |= rustix::fs::OFlags::DIRECTORY;
    }
    let file = File::from(
        rustix::fs::openat(parent, name, flags, rustix::fs::Mode::empty())
            .map_err(|_| Error::RouteNotFound)?,
    );
    let metadata = file.metadata()?;
    let owner = parent.metadata()?;
    if metadata.uid() != owner.uid()
        || metadata.dev() != owner.dev()
        || metadata.mode() & 0o022 != 0
        || if directory {
            !metadata.is_dir()
        } else {
            !metadata.is_file()
        }
    {
        return Err(Error::Denied);
    }
    Ok(file)
}

fn author(profile: &IdentityProfile, name: &str) -> Result<String> {
    let root = root(profile)?;
    let directory = child(&root, name, true)?;
    author_from_directory(&directory, name)
}

fn author_from_directory(directory: &File, name: &str) -> Result<String> {
    let mut file = child(directory, "manifest.json", false)?;
    let mut bytes = Vec::new();
    (&mut file).take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 1024 * 1024 {
        return Err(Error::Denied);
    }
    let manifest: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| Error::Denied)?;
    let author = manifest["package"]["author"]
        .as_str()
        .ok_or(Error::Denied)?;
    if manifest["package"]["name"] != name
        || dhttp_home::normalize_name(author).as_deref() != Some(author)
    {
        return Err(Error::Denied);
    }
    Ok(author.to_owned())
}

fn same_file(left: &Metadata, right: &Metadata) -> bool {
    left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

fn load(
    profile: &IdentityProfile,
    sandbox: &Arc<RwLock<Sandbox>>,
    expected_author: &str,
    name: &str,
) -> Result<Arc<Lib>> {
    let opened = (|| {
        let root = root(profile)?;
        let directory = child(&root, name, true)?;
        let file = child(&directory, "lib.wasm", false)?;
        let author = author_from_directory(&directory, name)?;
        if author != expected_author {
            return Err(Error::RouteNotFound);
        }
        Ok::<_, Error>((root, file, author))
    })();
    let (_root, mut file, author) = match opened {
        Ok(opened) => opened,
        Err(error) => {
            sandbox
                .write()
                .unwrap()
                .dev_libs
                .retain(|id, _| id.rsplit('/').next() != Some(name));
            return Err(error);
        }
    };
    let metadata = file.metadata()?;
    let id = format!("{author}/{name}");
    let mut state = sandbox.write().unwrap();
    if state.tasks.is_closed() {
        return Err(Error::Denied);
    }
    if let Some((previous, lib)) = state.dev_libs.get(&id)
        && same_file(previous, &metadata)
    {
        return Ok(lib.clone());
    }
    // Changed or removed files never fall back to stale executable code.
    state
        .dev_libs
        .retain(|id, _| id.rsplit('/').next() != Some(name));
    let mut bytes = Vec::new();
    (&mut file)
        .take(64 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if !same_file(&metadata, &file.metadata()?) {
        return Err(Error::InvalidComponent(
            "development file changed while reading".into(),
        ));
    }
    let data_root = profile.join(".volume");
    if !data_root.exists() {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(&data_root)?;
        if rustix::process::geteuid().is_root() {
            let directory = File::open(&data_root)?;
            let owner = profile.path().metadata()?;
            rustix::fs::fchown(
                &directory,
                Some(rustix::fs::Uid::from_raw(owner.uid())),
                Some(rustix::fs::Gid::from_raw(owner.gid())),
            )
            .map_err(std::io::Error::from)?;
        }
    }
    let data = File::from(
        rustix::fs::open(
            &data_root,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    );
    if data.metadata()?.uid() != profile.path().metadata()?.uid()
        || data.metadata()?.mode() & 0o022 != 0
    {
        return Err(Error::Denied);
    }
    let lib = Arc::new(Lib::load(
        state.runtime.clone(),
        id.clone(),
        &bytes,
        &data_root.join(if author == "std.dhttp.net" {
            format!("std/{name}")
        } else {
            name.to_owned()
        }),
    )?);
    state.dev_libs.insert(id, (metadata, lib.clone()));
    Ok(lib)
}

#[cfg(test)]
#[path = "../tests/unit/development.rs"]
mod tests;
