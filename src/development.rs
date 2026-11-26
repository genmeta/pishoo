//! Development routes backed by local files and protected by the identity router ACL.
use std::{
    collections::HashSet,
    fs::{File, Metadata},
    io::Read,
    os::unix::fs::MetadataExt,
    sync::{Arc, RwLock},
};

use axum::{Router, body::Body, response::IntoResponse, routing::any};
use dhttp_home::identity::IdentityProfile;
use http::Request;
use tower::ServiceExt;

use crate::{
    Error, Result, package,
    sandbox::{Lib, Sandbox},
};

pub(crate) fn preview_mount<'a>(
    profile: &IdentityProfile,
    tail: &'a str,
    frontend: bool,
) -> Option<(String, &'a str)> {
    std::iter::once(tail)
        .chain(tail.rmatch_indices('/').map(|(end, _)| &tail[..end]))
        .find_map(|mount| {
            [
                Some(mount.to_owned()),
                frontend.then(|| format!("std/{mount}")),
            ]
            .into_iter()
            .flatten()
            .find(|name| {
                package::valid_id(name).is_ok()
                    && profile
                        .join("lib")
                        .join(format!("{name}.preview/manifest.json"))
                        .is_file()
            })
            .map(|name| (name, &tail[mount.len()..]))
        })
}

pub(crate) fn router(
    profile: IdentityProfile,
    sandbox: Arc<RwLock<Sandbox>>,
    endpoint: dhttp::Endpoint,
    access: Arc<access_control::AccessService>,
    native_api: Router,
    fallback: Router,
) -> Router {
    let preview_api = Sandbox::api_router(sandbox.clone(), endpoint.clone(), access, native_api);
    let route = any(move |request: Request<Body>| {
        let (profile, sandbox, preview_api, fallback) = (
            profile.clone(),
            sandbox.clone(),
            preview_api.clone(),
            fallback.clone(),
        );
        async move {
            let app_request = request.uri().path().starts_with("/app/");
            let result: Result<axum::response::Response> = async {
                let path = request.uri().path();
                if path == "/sys" || path.starts_with("/sys/") {
                    return Ok(fallback
                        .oneshot(request)
                        .await
                        .expect("Router is infallible"));
                }
                let (frontend, standard, tail) = if let Some(tail) = path.strip_prefix("/app/") {
                    (true, false, tail)
                } else if let Some(tail) = path.strip_prefix("/std/") {
                    (false, true, tail)
                } else {
                    (false, false, path.trim_start_matches('/'))
                };
                if standard {
                    if !crate::routes::reserved_standard_path(&path["/std".len()..]) {
                        let profile = profile.clone();
                        let sandbox = sandbox.clone();
                        tokio::task::spawn_blocking(move || {
                            load_standard_libraries(&profile, &sandbox)
                        })
                        .await??;
                    }
                    return Ok(fallback
                        .oneshot(request)
                        .await
                        .expect("Router is infallible"));
                }
                let local = preview_mount(&profile, tail, frontend);
                let Some((name, suffix)) = local.filter(|(name, _)| frontend || name != "app")
                else {
                    sandbox.write().unwrap().dev_libs.retain(|id, (_, lib)| {
                        let package = id.split_once('/').map_or(id.as_str(), |(_, name)| name);
                        let keep = profile
                            .join("lib")
                            .join(format!("{package}.preview/manifest.json"))
                            .is_file();
                        if !keep {
                            lib.stream_retired.cancel();
                        }
                        keep
                    });
                    return Ok(fallback
                        .oneshot(request)
                        .await
                        .expect("Router is infallible"));
                };
                let suffix = suffix.to_owned();
                let author = author(&profile, &name)?;
                if !frontend && package::is_standard_author(&author) {
                    return Ok(fallback
                        .oneshot(request)
                        .await
                        .expect("Router is infallible"));
                }
                if frontend {
                    let mut response = tokio::task::spawn_blocking(move || {
                        let root = root(&profile)?;
                        let directory = project_directory(&root, &name)?;
                        let manifest = manifest(&directory, &name)?;
                        let app = runtime_file(&root, &directory, &name, &manifest, "app")?;
                        crate::sandbox::app_response(&app, &suffix, &request)
                    })
                    .await??;
                    response.headers_mut().insert(
                        http::header::CACHE_CONTROL,
                        http::HeaderValue::from_static("no-store"),
                    );
                    return Ok(response);
                }
                tokio::task::spawn_blocking(move || load(&profile, &sandbox, &author, &name))
                    .await??;
                Ok(preview_api
                    .oneshot(request)
                    .await
                    .expect("Router is infallible"))
            }
            .await;
            result.unwrap_or_else(|error| {
                let status = error.status();
                if status.is_server_error() {
                    tracing::error!(%error, "development request failed");
                }
                let mut response = (
                    status,
                    status.canonical_reason().unwrap_or("request failed"),
                )
                    .into_response();
                if app_request {
                    response.headers_mut().insert(
                        http::header::CACHE_CONTROL,
                        http::HeaderValue::from_static("no-store"),
                    );
                }
                response
            })
        }
    });
    Router::new().fallback(route)
}

// A shared lock protects file selection from Jade's atomic directory replacement.
// The file itself is the only lifecycle signal; no process/session registration exists.
fn root(profile: &IdentityProfile) -> Result<File> {
    let root = locked_root(profile)?;
    verify_root(profile, &root)?;
    Ok(root)
}

fn verify_root(profile: &IdentityProfile, root: &File) -> Result<()> {
    let metadata = root.metadata()?;
    if metadata.uid() != profile.path().metadata()?.uid() || metadata.mode() & 0o022 != 0 {
        return Err(Error::Denied);
    }
    Ok(())
}

fn locked_root(profile: &IdentityProfile) -> Result<File> {
    let root = File::from(
        rustix::fs::open(
            profile.path(),
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(|_| Error::RouteNotFound)?,
    );
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

fn project_directory(root: &File, name: &str) -> Result<File> {
    package::valid_id(name)?;
    format!("lib/{name}.preview")
        .split('/')
        .try_fold(root.try_clone()?, |parent, part| child(&parent, part, true))
}

fn author(profile: &IdentityProfile, name: &str) -> Result<String> {
    let root = root(profile)?;
    let directory = project_directory(&root, name)?;
    Ok(manifest(&directory, name)?["package"]["author"]
        .as_str()
        .unwrap()
        .to_owned())
}

fn manifest(directory: &File, name: &str) -> Result<serde_json::Value> {
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
    let version = manifest["package"]["version"]
        .as_str()
        .ok_or(Error::Denied)?;
    if manifest["package"]["name"] != name
        || dhttp_home::normalize_name(author).as_deref() != Some(author)
        || version.is_empty()
        || version.len() > 128
        || !version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+'))
    {
        return Err(Error::Denied);
    }
    Ok(manifest)
}

fn runtime_file(
    root: &File,
    directory: &File,
    name: &str,
    manifest: &serde_json::Value,
    kind: &str,
) -> Result<File> {
    let state = match child(directory, "state.json", false) {
        Ok(file) => Some(
            serde_json::from_reader::<_, serde_json::Value>(file.take(1024 * 1024))
                .map_err(|_| Error::Denied)?,
        ),
        Err(Error::RouteNotFound) => None,
        Err(error) => return Err(error),
    };
    let path = if kind == "lib" {
        let prefix = if package::is_standard_author(manifest["package"]["author"].as_str().unwrap())
            && !name.starts_with("std/")
        {
            "lib/std"
        } else {
            "lib"
        };
        format!(
            "{prefix}/{name}.wasm.{}",
            manifest["package"]["version"].as_str().unwrap()
        )
    } else {
        let name = package::app_name(name);
        format!(
            "app/{name}.{}",
            manifest["package"]["version"].as_str().unwrap()
        )
    };
    let open = |path: &str| -> Result<File> {
        let mut parts = path.split('/').peekable();
        let mut file = root.try_clone()?;
        while let Some(part) = parts.next() {
            file = child(&file, part, parts.peek().is_some() || kind == "app")?;
        }
        Ok(file)
    };
    let file = open(&path)?;
    if let Some(state) = state {
        let metadata = file.metadata()?;
        if state[kind]["dev"].as_u64() != Some(metadata.dev())
            || state[kind]["ino"].as_u64() != Some(metadata.ino())
        {
            return Err(Error::RouteNotFound);
        }
    }
    Ok(file)
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

// Refresh local standard Libs, then let the common API router select declarations.
fn load_standard_libraries(
    profile: &IdentityProfile,
    sandbox: &Arc<RwLock<Sandbox>>,
) -> Result<()> {
    // Discover marker names under Jade's lock. Only a selected preview needs
    // trusted ownership and modes along its entire path.
    let root = locked_root(profile)?;
    let open_directory = |parent: &File, name: &str| {
        use rustix::fs::{Mode, OFlags, openat};
        openat(
            parent,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map(File::from)
    };
    let directory = match open_directory(&root, "lib") {
        Ok(lib) => Some(lib),
        Err(rustix::io::Errno::NOENT) => None,
        Err(error) => return Err(std::io::Error::from(error).into()),
    };
    let mut names = Vec::new();
    let mut pending = directory
        .into_iter()
        .map(|directory| (String::new(), directory))
        .collect::<Vec<_>>();
    while let Some((prefix, directory)) = pending.pop() {
        for entry in cap_std::fs::Dir::from_std_file(directory.try_clone()?).entries()? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let Ok(leaf) = entry.file_name().into_string() else {
                continue;
            };
            let marker = leaf.strip_suffix(".preview");
            let segment = marker.unwrap_or(&leaf);
            let name = if prefix.is_empty() {
                segment.to_owned()
            } else {
                format!("{prefix}/{segment}")
            };
            if package::valid_id(&name).is_err() {
                continue;
            }
            if marker.is_none() {
                let nested = open_directory(&directory, &leaf).map_err(std::io::Error::from)?;
                pending.push((name, nested));
                continue;
            }
            names.push(name);
        }
    }
    drop(root);
    let mut present = HashSet::new();
    for name in names {
        let author = author(profile, &name)?;
        if package::is_standard_author(&author) {
            match load(profile, sandbox, &author, &name) {
                Ok(_) => {
                    present.insert(format!("{author}/{name}"));
                }
                Err(Error::RouteNotFound) => {}
                Err(error) => return Err(error),
            }
        }
    }
    sandbox.write().unwrap().dev_libs.retain(|id, (_, lib)| {
        let keep = !package::is_standard_author(id.split('/').next().unwrap_or(""))
            || present.contains(id);
        if !keep {
            lib.stream_retired.cancel();
        }
        keep
    });
    Ok(())
}

fn load(
    profile: &IdentityProfile,
    sandbox: &Arc<RwLock<Sandbox>>,
    expected_author: &str,
    name: &str,
) -> Result<Arc<Lib>> {
    let result = load_dependencies(profile, sandbox, expected_author, name, &mut HashSet::new());
    if result.is_err() {
        sandbox.write().unwrap().dev_libs.retain(|id, (_, lib)| {
            let keep = id.split_once('/').map_or(id.as_str(), |(_, name)| name) != name;
            if !keep {
                lib.stream_retired.cancel();
            }
            keep
        });
    }
    result
}

fn load_dependencies(
    profile: &IdentityProfile,
    sandbox: &Arc<RwLock<Sandbox>>,
    expected_author: &str,
    name: &str,
    visiting: &mut HashSet<String>,
) -> Result<Arc<Lib>> {
    visiting.insert(name.to_owned());
    let declared = {
        let root = root(profile)?;
        let directory = project_directory(&root, name)?;
        let manifest = manifest(&directory, name)?;
        manifest["dependencies"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    };
    let mut dependencies = std::collections::HashMap::new();
    for dependency in declared {
        let dependency_name = dependency["name"]
            .as_str()
            .ok_or_else(|| Error::InvalidComponent("missing local dependency name".into()))?;
        let version = dependency["version"]
            .as_str()
            .ok_or_else(|| Error::InvalidComponent("missing local dependency version".into()))?;
        package::valid_id(dependency_name)?;
        let (local_author, local_frontend) = {
            let root = root(profile)?;
            match project_directory(&root, dependency_name) {
                Ok(directory) => {
                    let local = manifest(&directory, dependency_name)?;
                    let author = local["package"]["author"].as_str().unwrap();
                    let expected = dependency["publisher"]
                        .as_str()
                        .or_else(|| dependency["author"].as_str());
                    let has_lib =
                        runtime_file(&root, &directory, dependency_name, &local, "lib").is_ok();
                    let matches = local["package"]["version"] == version
                        && expected.is_none_or(|expected| expected == author);
                    (
                        if matches && has_lib {
                            Some(author.to_owned())
                        } else {
                            None
                        },
                        matches
                            && !has_lib
                            && runtime_file(&root, &directory, dependency_name, &local, "app")
                                .is_ok(),
                    )
                }
                Err(Error::RouteNotFound) => (None, false),
                Err(error) => return Err(error),
            }
        };
        if let Some(author) = local_author {
            if !visiting.contains(dependency_name) {
                load_dependencies(profile, sandbox, &author, dependency_name, visiting)?;
            }
            dependencies.insert(
                dependency_name.to_owned(),
                (format!("{author}/{dependency_name}"), version.to_owned()),
            );
        } else {
            let target = sandbox
                .read()
                .unwrap()
                .versions
                .iter()
                .find(|((id, candidate_version), _)| {
                    id.split_once('/').map_or(id.as_str(), |(_, name)| name) == dependency_name
                        && candidate_version == version
                })
                .map(|(_, lib)| Arc::clone(lib));
            if let Some(target) = target {
                dependencies.insert(
                    dependency_name.to_owned(),
                    (target.id.clone(), target.openapi.info.version.clone()),
                );
            } else if !local_frontend {
                let installed = if profile.join(".volume/sys/setting/database").is_file() {
                    package::installed_package_versions(profile, None)?
                } else {
                    Vec::new()
                };
                if !installed.iter().any(|entry| {
                    entry["name"] == dependency_name
                        && entry["version"] == version
                        && entry["has_lib"] == false
                }) {
                    return Err(Error::InvalidComponent(format!(
                        "dependency {dependency_name} {version} is unavailable"
                    )));
                }
            }
        }
    }
    visiting.remove(name);
    load_library(profile, sandbox, expected_author, name, dependencies)
}

fn load_library(
    profile: &IdentityProfile,
    sandbox: &Arc<RwLock<Sandbox>>,
    expected_author: &str,
    name: &str,
    dependencies: std::collections::HashMap<String, (String, String)>,
) -> Result<Arc<Lib>> {
    // Installation takes the Sandbox lock before the identity directory lock.
    let mut state = sandbox.write().unwrap();
    if state.tasks.is_closed() {
        return Err(Error::Denied);
    }
    let opened = (|| {
        let root = root(profile)?;
        let directory = project_directory(&root, name)?;
        let manifest = manifest(&directory, name)?;
        let file = runtime_file(&root, &directory, name, &manifest, "lib")?;
        let author = manifest["package"]["author"].as_str().unwrap().to_owned();
        if author != expected_author {
            return Err(Error::RouteNotFound);
        }
        Ok::<_, Error>((root, file, author))
    })();
    let (_root, mut file, author) = match opened {
        Ok(opened) => opened,
        Err(error) => {
            state.dev_libs.retain(|id, (_, lib)| {
                let keep = id.split_once('/').map(|(_, name)| name).or(Some(&id[..])) != Some(name);
                if !keep {
                    lib.stream_retired.cancel();
                }
                keep
            });
            return Err(error);
        }
    };
    let metadata = file.metadata()?;
    let id = format!("{author}/{name}");
    if let Some((previous, lib)) = state.dev_libs.get(&id)
        && same_file(previous, &metadata)
        && lib.dependencies.len() == dependencies.len()
        && dependencies.iter().all(|(name, target)| {
            lib.dependencies
                .get(name)
                .is_some_and(|previous| previous == target)
        })
    {
        return Ok(lib.clone());
    }
    // Changed or removed files never fall back to stale executable code.
    state.dev_libs.retain(|id, (_, lib)| {
        let keep = id.split_once('/').map(|(_, name)| name).or(Some(&id[..])) != Some(name);
        if !keep {
            lib.stream_retired.cancel();
        }
        keep
    });
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
    let mut lib = Lib::load(
        state.runtime.clone(),
        id.clone(),
        &bytes,
        &data_root.join(
            if crate::package::is_standard_author(&author) && !name.starts_with("std/") {
                format!("std/{name}")
            } else {
                name.to_owned()
            },
        ),
    )?;
    lib.dependencies = dependencies;
    let lib = Arc::new(lib);
    state.dev_libs.insert(id, (metadata, lib.clone()));
    Ok(lib)
}

#[cfg(test)]
#[path = "../tests/unit/development.rs"]
mod tests;
