//! Lib files: directory traversal, queries, atomic installation and removal.
//! Opened directory handles refuse symlinks throughout traversal.

use std::os::unix::fs::MetadataExt;

use dhttp_home::identity::IdentityProfile;

use super::{WasmRuntime, check_lib, lib_metadata, validate_lib};
use crate::{Error, Result};

fn child_dir(parent: &std::fs::File, name: &std::ffi::OsStr) -> Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags, openat};
    openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(std::fs::File::from)
    .map_err(layout_error)
}

fn layout_error(error: rustix::io::Errno) -> Error {
    let error = if error == rustix::io::Errno::LOOP || error == rustix::io::Errno::NOTDIR {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "resource layout must contain real directories and regular files",
        )
    } else {
        std::io::Error::from(error)
    };
    Error::Io(error)
}

fn profile_dir(profile: &IdentityProfile) -> Result<std::fs::File> {
    let parent = std::fs::File::open(
        profile
            .path()
            .parent()
            .ok_or_else(|| Error::BadRequest("invalid identity directory".into()))?,
    )?;
    child_dir(
        &parent,
        profile
            .path()
            .file_name()
            .ok_or_else(|| Error::BadRequest("invalid identity directory".into()))?,
    )
}

pub(super) fn lib_root(profile: &IdentityProfile) -> Result<std::fs::File> {
    child_dir(&profile_dir(profile)?, std::ffi::OsStr::new("lib"))
}

pub(super) fn valid_id(id: &str) -> Result<()> {
    let name = match id.split_once('/') {
        Some((publisher, name))
            if dhttp_home::normalize_name(publisher).as_deref() == Some(publisher) =>
        {
            name
        }
        Some(_) => return Err(Error::BadRequest("invalid Lib publisher".into())),
        None => id,
    };
    if name.len() > 63
        || !name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(Error::BadRequest("invalid Lib id".into()));
    }
    Ok(())
}

fn resource_dir(root: &std::fs::File, id: &str) -> Result<std::fs::File> {
    valid_id(id)?;
    match id.split_once('/') {
        Some((publisher, name)) => {
            let publisher = child_dir(root, std::ffi::OsStr::new(publisher))?;
            child_dir(&publisher, std::ffi::OsStr::new(name))
        }
        None => child_dir(root, std::ffi::OsStr::new(id)),
    }
}

// Called under the root's exclusive commit lock. The returned handle is the
// actual target parent; no unchecked slash-containing path reaches openat.
fn resource_parent(root: &std::fs::File, id: &str, create: bool) -> Result<std::fs::File> {
    match id.split_once('/') {
        Some((publisher, _)) => {
            if create {
                use rustix::fs::{Mode, mkdirat};
                match mkdirat(root, publisher, Mode::from_raw_mode(0o700)) {
                    Ok(()) => root.sync_all()?,
                    Err(rustix::io::Errno::EXIST) => {}
                    Err(error) => return Err(layout_error(error)),
                }
            }
            child_dir(root, std::ffi::OsStr::new(publisher))
        }
        None => Ok(root.try_clone()?),
    }
}

fn check_resource_identity(openapi: &oas3::OpenApiV3Spec, id: &str) -> Result<()> {
    if let Some((publisher, name)) = id.split_once('/') {
        let document = serde_json::to_value(openapi)
            .map_err(|error| Error::InvalidComponent(error.to_string()))?;
        if document["x-pishoo"]["publisher"] != publisher || document["x-pishoo"]["name"] != name {
            return Err(Error::InvalidComponent(
                "Lib ID and manifest identity disagree".into(),
            ));
        }
    }
    Ok(())
}

fn signed_owner(bytes: &[u8]) -> Result<Option<dhttp_home::certificate::OwnerHash>> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let mut depth = 0;
    for payload in wasmparser::Parser::new(0).parse_all(bytes) {
        match payload.map_err(|error| Error::InvalidComponent(error.to_string()))? {
            wasmparser::Payload::ModuleSection { .. }
            | wasmparser::Payload::ComponentSection { .. } => depth += 1,
            wasmparser::Payload::End(_) if depth > 0 => depth -= 1,
            wasmparser::Payload::CustomSection(section)
                if depth == 0 && section.name() == "pishoo:signature" =>
            {
                validate_lib(bytes)?;
                let envelope: serde_json::Value = serde_json::from_slice(section.data())
                    .map_err(|error| Error::InvalidComponent(error.to_string()))?;
                let certificate = STANDARD
                    .decode(envelope["certificates"][0].as_str().ok_or_else(|| {
                        Error::InvalidComponent("missing signer certificate".into())
                    })?)
                    .map_err(|_| Error::InvalidComponent("invalid signer certificate".into()))?;
                let ski = dhttp_home::certificate::extract_dhttp_subject_key_identifier(&[
                    qtls::CertificateDer::from(certificate),
                ])
                .map_err(|_| Error::InvalidComponent("invalid signer owner_hash".into()))?;
                return Ok(Some(ski.owner_hash().clone()));
            }
            _ => {}
        }
    }
    Ok(None)
}

fn check_publisher_update(dir: &std::fs::File, id: &str, bytes: &[u8]) -> Result<()> {
    use std::io::Read;
    if !id.contains('/') {
        return Ok(());
    }
    let file = match component_file(dir) {
        Ok(file) => file,
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let mut previous = Vec::new();
    file.take(64 * 1024 * 1024 + 1).read_to_end(&mut previous)?;
    // An unsigned/corrupt legacy file has no authenticated publisher owner to
    // inherit. A valid signed resource cannot be replaced by another owner.
    if let Ok(Some(owner)) = signed_owner(&previous)
        && signed_owner(bytes)?.as_ref() != Some(&owner)
    {
        return Err(Error::InvalidComponent(
            "publisher owner_hash changed; ordinary update is refused".into(),
        ));
    }
    Ok(())
}

pub(super) fn disk_ids(root: &std::fs::File) -> Result<Vec<String>> {
    let dir = cap_std::fs::Dir::from_std_file(root.try_clone()?);
    let mut ids = Vec::new();
    for entry in dir.entries()? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "symlink in lib layout",
            )));
        }
        if !kind.is_dir() {
            continue;
        }
        let id = entry
            .file_name()
            .into_string()
            .map_err(|_| Error::BadRequest("invalid Lib id".into()))?;
        if id.contains('.') && dhttp_home::normalize_name(&id).as_deref() == Some(&id) {
            let publisher = child_dir(root, std::ffi::OsStr::new(&id))?;
            let publisher = cap_std::fs::Dir::from_std_file(publisher);
            for entry in publisher.entries()? {
                let entry = entry?;
                let kind = entry.file_type()?;
                if kind.is_symlink() {
                    return Err(Error::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "symlink in publisher layout",
                    )));
                }
                if kind.is_dir() {
                    let name = entry
                        .file_name()
                        .into_string()
                        .map_err(|_| Error::BadRequest("invalid Lib name".into()))?;
                    let full = format!("{id}/{name}");
                    valid_id(&full)?;
                    ids.push(full);
                }
            }
        } else {
            valid_id(&id)?;
            ids.push(id);
        }
    }
    ids.sort();
    Ok(ids)
}

fn component_file(dir: &std::fs::File) -> Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags, openat};
    let file = std::fs::File::from(
        openat(
            dir,
            "lib.wasm",
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(layout_error)?,
    );
    if !file.metadata()?.is_file() {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "lib.wasm must be a regular file",
        )));
    }
    Ok(file)
}

pub(super) fn disk_bytes(root: &std::fs::File, id: &str) -> Result<Vec<u8>> {
    use std::io::Read;
    valid_id(id)?;
    let dir = resource_dir(root, id)?;
    let mut bytes = Vec::new();
    component_file(&dir)?
        .take(64 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 64 * 1024 * 1024 {
        return Err(Error::InvalidComponent("component exceeds 64 MiB".into()));
    }
    Ok(bytes)
}

pub(crate) fn installed_libs(
    profile: &IdentityProfile,
    id: Option<&str>,
) -> Result<serde_json::Value> {
    if let Some(id) = id {
        valid_id(id)?;
    }
    let root = lib_root(profile)?;
    rustix::fs::flock(&root, rustix::fs::FlockOperation::LockShared)
        .map_err(std::io::Error::from)?;
    if let Some(id) = id {
        let bytes = disk_bytes(&root, id).map_err(|error| match error {
            Error::Io(ref io) if io.kind() == std::io::ErrorKind::NotFound => Error::RouteNotFound,
            _ => error,
        })?;
        let openapi = validate_lib(&bytes)?;
        check_resource_identity(&openapi, id)?;
        return Ok(lib_metadata(&openapi, Some(id)));
    }
    let mut values = Vec::new();
    for id in disk_ids(&root)? {
        let value = match disk_bytes(&root, &id).and_then(|bytes| {
            let openapi = validate_lib(&bytes)?;
            check_resource_identity(&openapi, &id)?;
            Ok(openapi)
        }) {
            Ok(openapi) => lib_metadata(&openapi, Some(&id)),
            Err(Error::InvalidComponent(message)) => serde_json::json!({"id":id,"error":message}),
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                serde_json::json!({"id":id,"error":"missing lib.wasm"})
            }
            Err(error) => return Err(error),
        };
        values.push(value);
    }
    Ok(values.into())
}

fn initialized_profile(profile: &IdentityProfile) -> Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags, openat};
    let dir = profile_dir(profile)?;
    let db = child_dir(&dir, std::ffi::OsStr::new("db"))?;
    let file = std::fs::File::from(
        openat(
            &db,
            "config.db",
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(layout_error)?,
    );
    if !file.metadata()?.is_file() || file.metadata()?.len() == 0 {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "identity is not initialized; run pishoo first",
        )));
    }
    crate::setup::config_database(
        profile,
        &http::Method::GET,
        &http::Uri::from_static("/std/pishoo/settings"),
        None,
    )?;
    Ok(dir)
}

fn check_component_target(dir: &std::fs::File) -> Result<()> {
    match component_file(dir) {
        Ok(_) => Ok(()),
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn check_lib_root(
    profile: &IdentityProfile,
    parent: &std::fs::File,
    root: &std::fs::File,
) -> Result<()> {
    let current_parent = profile_dir(profile)?;
    let current_root = child_dir(parent, std::ffi::OsStr::new("lib"))?;
    if parent.metadata()?.dev() != current_parent.metadata()?.dev()
        || parent.metadata()?.ino() != current_parent.metadata()?.ino()
        || root.metadata()?.dev() != current_root.metadata()?.dev()
        || root.metadata()?.ino() != current_root.metadata()?.ino()
    {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "identity or lib directory changed; retry the operation",
        )));
    }
    Ok(())
}

fn temporary_name() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(std::io::Error::other)?;
    Ok(format!(
        ".pishoo-{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ))
}

fn write_component(dir: &std::fs::File, name: &str, bytes: &[u8]) -> Result<()> {
    use std::io::Write;

    use rustix::fs::{Mode, OFlags, openat};
    let file = std::fs::File::from(
        openat(
            dir,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(layout_error)?,
    );
    let mut file = scopeguard::guard(file, |_| {
        let _ = rustix::fs::unlinkat(dir, name, rustix::fs::AtFlags::empty());
    });
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(scopeguard::ScopeGuard::into_inner(file));
    Ok(())
}

pub(crate) fn install_lib(
    profile: &IdentityProfile,
    id: &str,
    bytes: &[u8],
    runtime: &WasmRuntime,
) -> Result<serde_json::Value> {
    use rustix::fs::{AtFlags, FlockOperation, Mode, flock, mkdirat, renameat, unlinkat};
    valid_id(id)?;
    let parent = initialized_profile(profile)?;
    let root = child_dir(&parent, std::ffi::OsStr::new("lib"))?;
    // Inspect layout before expensive compilation, then release the read lock.
    flock(&root, FlockOperation::LockShared).map_err(std::io::Error::from)?;
    match resource_dir(&root, id) {
        Ok(dir) => check_component_target(&dir)?,
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    flock(&root, FlockOperation::Unlock).map_err(std::io::Error::from)?;
    let openapi = check_lib(bytes, runtime)?;
    check_resource_identity(&openapi, id)?;
    // No guest/data grant is created. Prepare under a shared lock, then recheck
    // under the exclusive commit lock. A competing removal/install retries only
    // preparation, using the same validated bytes and compilation result.
    loop {
        flock(&root, FlockOperation::LockShared).map_err(std::io::Error::from)?;
        let name = temporary_name()?;
        match resource_dir(&root, id) {
            Ok(dir) => {
                check_component_target(&dir)?;
                write_component(&dir, &name, bytes)?;
                let cleanup = scopeguard::guard((), |_| {
                    let _ = unlinkat(&dir, &name, AtFlags::empty());
                });
                flock(&root, FlockOperation::Unlock).map_err(std::io::Error::from)?;
                flock(&root, FlockOperation::LockExclusive).map_err(std::io::Error::from)?;
                check_lib_root(profile, &parent, &root)?;
                let current = match resource_dir(&root, id) {
                    Ok(current) => current,
                    Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                        drop(cleanup);
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                if dir.metadata()?.dev() != current.metadata()?.dev()
                    || dir.metadata()?.ino() != current.metadata()?.ino()
                {
                    drop(cleanup);
                    continue;
                }
                check_component_target(&current)?;
                check_publisher_update(&current, id, bytes)?;
                renameat(&dir, &name, &dir, "lib.wasm").map_err(layout_error)?;
                dir.sync_all()?;
                drop(cleanup);
                break;
            }
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                if parent.metadata()?.dev() != root.metadata()?.dev() {
                    return Err(Error::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "lib root must share the identity filesystem for atomic publication",
                    )));
                }
                let cleanup_dir = cap_std::fs::Dir::from_std_file(parent.try_clone()?);
                mkdirat(&parent, &name, Mode::from_raw_mode(0o700)).map_err(layout_error)?;
                let cleanup = scopeguard::guard((), |_| {
                    let _ = cleanup_dir.remove_dir_all(&name);
                });
                let stage = child_dir(&parent, std::ffi::OsStr::new(&name))?;
                write_component(&stage, "lib.wasm", bytes)?;
                stage.sync_all()?;
                flock(&root, FlockOperation::Unlock).map_err(std::io::Error::from)?;
                flock(&root, FlockOperation::LockExclusive).map_err(std::io::Error::from)?;
                check_lib_root(profile, &parent, &root)?;
                match resource_dir(&root, id) {
                    Ok(_) => {
                        drop(cleanup);
                        continue;
                    }
                    Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
                let target_parent = resource_parent(&root, id, true)?;
                if target_parent.metadata()?.dev() != parent.metadata()?.dev() {
                    return Err(Error::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "Lib publisher directory must share the identity filesystem",
                    )));
                }
                let leaf = id.rsplit('/').next().unwrap();
                renameat(&parent, &name, &target_parent, leaf).map_err(layout_error)?;
                target_parent.sync_all()?;
                root.sync_all()?;
                parent.sync_all()?;
                drop(cleanup);
                break;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(lib_metadata(&openapi, Some(id)))
}

pub(crate) fn remove_lib(profile: &IdentityProfile, id: &str) -> Result<()> {
    use rustix::fs::{FlockOperation, Mode, flock, mkdirat, renameat};
    valid_id(id)?;
    let parent = initialized_profile(profile)?;
    let root = child_dir(&parent, std::ffi::OsStr::new("lib"))?;
    flock(&root, FlockOperation::LockExclusive).map_err(std::io::Error::from)?;
    check_lib_root(profile, &parent, &root)?;
    match resource_dir(&root, id) {
        Ok(dir) => {
            check_component_target(&dir)?;
            if parent.metadata()?.dev() != root.metadata()?.dev() {
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "lib root must share the identity filesystem for atomic removal",
                )));
            }
            let name = temporary_name()?;
            let cleanup_dir = cap_std::fs::Dir::from_std_file(parent.try_clone()?);
            mkdirat(&parent, &name, Mode::from_raw_mode(0o700)).map_err(layout_error)?;
            let cleanup = scopeguard::guard((), |_| {
                let _ = cleanup_dir.remove_dir_all(&name);
            });
            let stage = child_dir(&parent, std::ffi::OsStr::new(&name))?;
            let target_parent = resource_parent(&root, id, false)?;
            let leaf = id.rsplit('/').next().unwrap();
            renameat(&target_parent, leaf, &stage, "lib").map_err(layout_error)?;
            target_parent.sync_all()?;
            root.sync_all()?;
            stage.sync_all()?;
            parent.sync_all()?;
            cleanup_dir.remove_dir_all(&name)?;
            drop(cleanup);
            parent.sync_all()?;
            Ok(())
        }
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}
