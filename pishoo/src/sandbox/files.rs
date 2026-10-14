//! Lib files: directory traversal, queries, atomic installation and removal.
//! Opened directory handles refuse symlinks throughout traversal.

use std::os::unix::fs::MetadataExt;

use dhttp_home::identity::IdentityProfile;

use super::{WasmRuntime, check_lib, lib_metadata, package_metadata, read_package};
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

fn check_resource_identity(metadata: &serde_json::Value, id: &str) -> Result<()> {
    if let Some((publisher, name)) = id.split_once('/') {
        let document = metadata;
        if document["publisher"] != publisher || document["name"] != name {
            return Err(Error::InvalidComponent(
                "Lib ID and manifest identity disagree".into(),
            ));
        }
    }
    Ok(())
}

fn signed_owner(bytes: &[u8]) -> Result<Option<dhttp_home::certificate::OwnerHash>> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let package = read_package(bytes)?;
    let volume = package.get_volume("pishoo-signature").unwrap();
    let (data, _) = volume.read_file("/signature.json").unwrap();
    let envelope: serde_json::Value = serde_json::from_slice(&data)
        .map_err(|error| Error::InvalidComponent(error.to_string()))?;
    let certificate = STANDARD
        .decode(envelope["certificates"][0].as_str().unwrap())
        .map_err(|_| Error::InvalidComponent("invalid signer certificate".into()))?;
    let ski = dhttp_home::certificate::extract_dhttp_subject_key_identifier(&[
        qtls::CertificateDer::from(certificate),
    ])
    .map_err(|_| Error::InvalidComponent("invalid signer owner_hash".into()))?;
    Ok(Some(ski.owner_hash().clone()))
}

fn check_publisher_update(dir: &std::fs::File, id: &str, bytes: &[u8], kind: &str) -> Result<()> {
    use std::io::Read;
    if !id.contains('/') {
        return Ok(());
    }
    let file = match artifact_file(dir, kind) {
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

// Installation facts belong to config.db. Signed files are verified against
// these rows, never discovered as an implicit installation during startup.
fn installation_database(profile: &IdentityProfile, create: bool) -> Result<rusqlite::Connection> {
    let path = profile.config_db_path();
    if !path.symlink_metadata()?.file_type().is_file() {
        return Err(Error::InvalidConfig(
            "config.db must be a regular file".into(),
        ));
    }
    let mut conn = rusqlite::Connection::open_with_flags(
        path,
        if create {
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
        } else {
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
        },
    )?;
    conn.busy_timeout(std::time::Duration::from_secs(3))?;
    conn.pragma_update(None, "foreign_keys", true)?;
    if create {
        conn.pragma_update(None, "synchronous", "FULL")?;
        let ready: bool = conn.query_row("SELECT count(*)=4 FROM sqlite_master WHERE type='table' AND name IN ('libs','apps','app_dependencies','lib_dependencies')",[],|row|row.get(0))?;
        if !ready {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            tx.execute_batch(include_str!("install.sql"))?;
            tx.commit()?;
        }
    }
    Ok(conn)
}

fn registered_packages(
    conn: &rusqlite::Connection,
    kind: &str,
    id: Option<&str>,
) -> Result<Vec<(String, String)>> {
    let table = if kind == "app" { "apps" } else { "libs" };
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |row| row.get(0),
    )?;
    let mut records = Vec::new();
    if exists {
        let mut stmt = conn.prepare(&format!(
            "SELECT id,name,version FROM {table} WHERE ?1 IS NULL OR id=?1 ORDER BY id"
        ))?;
        for row in stmt.query_map([id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })? {
            let (id, name, version) = row?;
            valid_id(&id)?;
            if id.rsplit('/').next() != Some(name.as_str())
                || records.iter().any(|(other, _): &(String, String)| {
                    other.rsplit('/').next() == Some(name.as_str())
                })
            {
                return Err(Error::InvalidConfig(
                    "invalid or conflicting installed package name".into(),
                ));
            }
            records.push((id, version));
        }
    }
    if id.is_some() && records.is_empty() {
        return Err(Error::RouteNotFound);
    }
    Ok(records)
}

fn check_name(conn: &rusqlite::Connection, id: &str, kind: &str) -> Result<()> {
    use rusqlite::OptionalExtension;
    let table = if kind == "app" { "apps" } else { "libs" };
    let other: Option<String> = conn
        .query_row(
            &format!("SELECT id FROM {table} WHERE name=?1 AND id<>?2"),
            rusqlite::params![id.rsplit('/').next().unwrap(), id],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(other) = other {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("name is already installed as {other}; cannot install {id}"),
        )));
    }
    Ok(())
}

fn check_proxy_mount(conn: &rusqlite::Connection, id: &str, kind: &str) -> Result<()> {
    let prefix = format!(
        "/{}/{}",
        if kind == "app" { "app" } else { "api" },
        id.rsplit('/').next().unwrap()
    );
    let mut stmt = conn.prepare("SELECT location FROM proxy_locations")?;
    for location in stmt.query_map([], |row| row.get::<_, String>(0))? {
        let location = location?;
        let path = location.strip_prefix("= ").unwrap_or(&location);
        if path == prefix
            || path
                .strip_prefix(&prefix)
                .is_some_and(|tail| tail.starts_with('/'))
        {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("cannot install {id}: proxy {location} occupies {prefix}"),
            )));
        }
    }
    Ok(())
}

fn save_registration(
    conn: &rusqlite::Connection,
    id: &str,
    kind: &str,
    api: &serde_json::Value,
) -> Result<()> {
    check_name(conn, id, kind)?;
    check_proxy_mount(conn, id, kind)?;
    if kind == "lib" {
        let incompatible: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM app_dependencies WHERE lib_id=?1 AND version<>?2) OR EXISTS(SELECT 1 FROM lib_dependencies WHERE dependency_id=?1 AND version<>?2)",rusqlite::params![id,api["version"].as_str().unwrap()],|row|row.get(0))?;
        if incompatible {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "lib version is required by installed dependencies",
            )));
        }
    }
    let table = if kind == "app" { "apps" } else { "libs" };
    conn.execute(&format!("INSERT INTO {table}(id,name,version) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET name=excluded.name,version=excluded.version"),
        rusqlite::params![id,id.rsplit('/').next().unwrap(),api["version"].as_str().unwrap()])?;
    let (edges, source, target) = if kind == "app" {
        ("app_dependencies", "app_id", "lib_id")
    } else {
        ("lib_dependencies", "lib_id", "dependency_id")
    };
    conn.execute(&format!("DELETE FROM {edges} WHERE {source}=?1"), [id])?;
    for (dependency, version) in declared_dependencies(api) {
        let matches: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM libs WHERE id=?1 AND version=?2)",
            rusqlite::params![dependency, version],
            |row| row.get(0),
        )?;
        if !matches {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("dependency {dependency}@{version} is not installed"),
            )));
        }
        if kind == "lib" {
            check_dependency_cycle(conn, id, &dependency)?;
        }
        conn.execute(
            &format!("INSERT INTO {edges}({source},{target},version) VALUES(?1,?2,?3)"),
            rusqlite::params![id, dependency, version],
        )?;
    }

    Ok(())
}

fn declared_dependencies(api: &serde_json::Value) -> Vec<(String, String)> {
    let document = api;
    document["dependencies"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|dep| {
            (
                format!(
                    "{}/{}",
                    dep["publisher"].as_str().unwrap(),
                    dep["name"].as_str().unwrap()
                ),
                dep["version"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn check_dependency_cycle(conn: &rusqlite::Connection, id: &str, dependency: &str) -> Result<()> {
    let mut pending = vec![dependency.to_owned()];
    let mut seen = std::collections::HashSet::new();
    while let Some(current) = pending.pop() {
        if current == id {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("cyclic backend dependency involving {id}"),
            )));
        }
        if !seen.insert(current.clone()) {
            continue;
        }
        pending.extend(
            conn.prepare("SELECT dependency_id FROM lib_dependencies WHERE lib_id=?1")?
                .query_map([&current], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?,
        );
    }
    Ok(())
}

fn check_registration_dependencies(
    conn: &rusqlite::Connection,
    id: &str,
    kind: &str,
    api: &serde_json::Value,
) -> Result<()> {
    let mut expected = declared_dependencies(api);
    expected.sort();
    let (edges, source, target) = if kind == "app" {
        ("app_dependencies", "app_id", "lib_id")
    } else {
        ("lib_dependencies", "lib_id", "dependency_id")
    };
    let actual = conn
        .prepare(&format!(
            "SELECT {target},version FROM {edges} WHERE {source}=?1 ORDER BY {target}"
        ))?
        .query_map([id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if actual != expected {
        return Err(Error::InvalidComponent(format!(
            "registered dependencies differ from file for {id}"
        )));
    }
    for (dependency, version) in expected {
        let matches: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM libs WHERE id=?1 AND version=?2)",
            rusqlite::params![dependency, version],
            |row| row.get(0),
        )?;
        if !matches {
            return Err(Error::InvalidComponent(format!(
                "registered dependency {dependency}@{version} is missing or incompatible"
            )));
        }
        if kind == "lib" {
            check_dependency_cycle(conn, id, &dependency)?;
        }
    }
    Ok(())
}

fn artifact_file(dir: &std::fs::File, kind: &str) -> Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags, openat};
    let file = std::fs::File::from(
        openat(
            dir,
            format!("{kind}.webc"),
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(layout_error)?,
    );
    if !file.metadata()?.is_file() {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "lib.webc must be a regular file",
        )));
    }
    Ok(file)
}

pub(super) fn disk_bytes(root: &std::fs::File, id: &str) -> Result<Vec<u8>> {
    artifact_bytes(root, id, "lib")
}

fn artifact_bytes(root: &std::fs::File, id: &str, kind: &str) -> Result<Vec<u8>> {
    use std::io::Read;
    valid_id(id)?;
    let dir = resource_dir(root, id)?;
    let mut bytes = Vec::new();
    artifact_file(&dir, kind)?
        .take(64 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 64 * 1024 * 1024 {
        return Err(Error::InvalidComponent(
            "WEBC package exceeds 64 MiB".into(),
        ));
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
    let root = match lib_root(profile) {
        Ok(root) => root,
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            let conn = installation_database(profile, false)?;
            if registered_packages(&conn, "lib", id)?.is_empty() {
                return Ok(serde_json::json!([]));
            }
            return Err(Error::Io(e));
        }
        Err(e) => return Err(e),
    };
    rustix::fs::flock(&root, rustix::fs::FlockOperation::LockShared)
        .map_err(std::io::Error::from)?;
    let conn = installation_database(profile, false)?;
    let mut values = Vec::new();
    for (registered, version) in registered_packages(&conn, "lib", id)? {
        let value = match disk_bytes(&root, &registered).and_then(|bytes| {
            let package = read_package(&bytes)?;
            let metadata = package_metadata(&package)?;
            check_resource_identity(&metadata, &registered)?;
            if metadata["version"].as_str().unwrap() != version {
                return Err(Error::InvalidComponent(format!(
                    "registered version differs from file for {registered}"
                )));
            }
            check_registration_dependencies(&conn, &registered, "lib", &metadata)?;
            Ok(package)
        }) {
            Ok(package) => lib_metadata(&package, Some(&registered)),
            Err(error) if id.is_some() => return Err(error),
            Err(error) => serde_json::json!({"id":registered,"error":error.to_string()}),
        };
        values.push(value);
    }
    if id.is_some() {
        Ok(values.pop().unwrap())
    } else {
        Ok(values.into())
    }
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

fn check_component_target(dir: &std::fs::File, kind: &str) -> Result<()> {
    match artifact_file(dir, kind) {
        Ok(_) => Ok(()),
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn check_lib_root(
    profile: &IdentityProfile,
    parent: &std::fs::File,
    root: &std::fs::File,
    kind: &str,
) -> Result<()> {
    let current_parent = profile_dir(profile)?;
    let current_root = child_dir(parent, std::ffi::OsStr::new(kind))?;
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
    install_artifact(profile, id, bytes, Some(runtime), "lib")
}

pub(crate) fn install_app(
    profile: &IdentityProfile,
    id: &str,
    bytes: &[u8],
) -> Result<serde_json::Value> {
    install_artifact(profile, id, bytes, None, "app")
}

fn install_artifact(
    profile: &IdentityProfile,
    id: &str,
    bytes: &[u8],
    runtime: Option<&WasmRuntime>,
    kind: &str,
) -> Result<serde_json::Value> {
    use rustix::fs::{AtFlags, FlockOperation, Mode, flock, mkdirat, renameat, unlinkat};
    valid_id(id)?;
    if kind == "app" && !id.contains('/') {
        return Err(Error::BadRequest(
            "frontend app requires publisher/name ID".into(),
        ));
    }
    let parent = initialized_profile(profile)?;
    flock(&parent, FlockOperation::LockExclusive).map_err(std::io::Error::from)?;
    if kind == "app" {
        match mkdirat(&parent, "app", Mode::from_raw_mode(0o700)) {
            Ok(()) => parent.sync_all()?,
            Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(layout_error(error)),
        }
    }
    let root = child_dir(&parent, std::ffi::OsStr::new(kind))?;
    // Layout inspection is only part of an explicit install, never discovery.
    flock(&root, FlockOperation::LockShared).map_err(std::io::Error::from)?;
    disk_ids(&root)?;
    let mut database = installation_database(profile, true)?;
    check_name(&database, id, kind)?;
    check_proxy_mount(&database, id, kind)?;
    match resource_dir(&root, id) {
        Ok(dir) => check_component_target(&dir, kind)?,
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    flock(&root, FlockOperation::Unlock).map_err(std::io::Error::from)?;
    let package = read_package(bytes)?;
    let metadata = package_metadata(&package)?;
    if metadata["kind"] != kind {
        return Err(Error::InvalidComponent(
            "package kind differs from install target".into(),
        ));
    }
    if let Some(runtime) = runtime {
        check_lib(bytes, runtime)?;
    }
    check_resource_identity(&metadata, id)?;
    if kind == "lib" {
        let dependents: i64 = database.query_row("SELECT (SELECT count(*) FROM app_dependencies WHERE lib_id=?1)+(SELECT count(*) FROM lib_dependencies WHERE dependency_id=?1)",[id],|row|row.get(0))?;
        if dependents > 0 {
            let previous = artifact_bytes(&root, id, "lib")?;
            if previous != bytes {
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "lib contents/version are required by installed App/Lib dependencies",
                )));
            }
        }
    }
    for (dep_id, version) in declared_dependencies(&metadata) {
        let installed = installed_libs(profile, Some(&dep_id))?;
        if installed["version"] != version {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("dependency {dep_id} requires {version}"),
            )));
        }
    }
    // No guest/data grant is created. Prepare under a shared lock, then recheck
    // under the exclusive commit lock. A competing removal/install retries only
    // preparation, using the same validated bytes and compilation result.
    loop {
        flock(&root, FlockOperation::LockShared).map_err(std::io::Error::from)?;
        let name = temporary_name()?;
        match resource_dir(&root, id) {
            Ok(dir) => {
                check_component_target(&dir, kind)?;
                write_component(&dir, &name, bytes)?;
                let cleanup = scopeguard::guard((), |_| {
                    let _ = unlinkat(&dir, &name, AtFlags::empty());
                });
                flock(&root, FlockOperation::Unlock).map_err(std::io::Error::from)?;
                flock(&root, FlockOperation::LockExclusive).map_err(std::io::Error::from)?;
                check_lib_root(profile, &parent, &root, kind)?;
                check_name(&database, id, kind)?;
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
                check_component_target(&current, kind)?;
                check_publisher_update(&current, id, bytes, kind)?;
                let tx =
                    database.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                save_registration(&tx, id, kind, &metadata)?;
                let artifact = format!("{kind}.webc");
                let backup = temporary_name()?;
                let previous =
                    match rustix::fs::linkat(&dir, &artifact, &dir, &backup, AtFlags::empty()) {
                        Ok(()) => Some(backup),
                        Err(rustix::io::Errno::NOENT) => None,
                        Err(error) => return Err(layout_error(error)),
                    };
                let rollback = scopeguard::guard(previous, |previous| {
                    if let Some(backup) = previous {
                        let _ = renameat(&dir, backup, &dir, &artifact);
                    } else {
                        let _ = unlinkat(&dir, &artifact, AtFlags::empty());
                    }
                    let _ = dir.sync_all();
                });
                renameat(&dir, &name, &dir, &artifact).map_err(layout_error)?;
                dir.sync_all()?;
                tx.commit()?;
                if let Some(backup) = scopeguard::ScopeGuard::into_inner(rollback) {
                    let _ = unlinkat(&dir, backup, AtFlags::empty());
                }
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
                write_component(&stage, &format!("{kind}.webc"), bytes)?;
                stage.sync_all()?;
                flock(&root, FlockOperation::Unlock).map_err(std::io::Error::from)?;
                flock(&root, FlockOperation::LockExclusive).map_err(std::io::Error::from)?;
                check_lib_root(profile, &parent, &root, kind)?;
                check_name(&database, id, kind)?;
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
                let tx =
                    database.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                save_registration(&tx, id, kind, &metadata)?;
                renameat(&parent, &name, &target_parent, leaf).map_err(layout_error)?;
                let rollback = scopeguard::guard((), |_| {
                    let _ = renameat(&target_parent, leaf, &parent, &name);
                    let _ = target_parent.sync_all();
                });
                target_parent.sync_all()?;
                root.sync_all()?;
                parent.sync_all()?;
                tx.commit()?;
                scopeguard::ScopeGuard::into_inner(rollback);
                drop(cleanup);
                break;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(lib_metadata(&package, Some(id)))
}

pub(crate) fn remove_lib(profile: &IdentityProfile, id: &str) -> Result<()> {
    remove_artifact(profile, id, "lib")
}

pub(crate) fn remove_app(profile: &IdentityProfile, id: &str) -> Result<()> {
    remove_artifact(profile, id, "app")
}

fn remove_artifact(profile: &IdentityProfile, id: &str, kind: &str) -> Result<()> {
    use rustix::fs::{FlockOperation, Mode, flock, mkdirat, renameat};
    valid_id(id)?;
    let parent = initialized_profile(profile)?;
    flock(&parent, FlockOperation::LockExclusive).map_err(std::io::Error::from)?;
    let mut database = installation_database(profile, true)?;
    match registered_packages(&database, kind, Some(id)) {
        Ok(_) => {}
        Err(Error::RouteNotFound) => return Ok(()),
        Err(e) => return Err(e),
    }
    if kind == "lib" {
        let dependents: i64 = database.query_row("SELECT (SELECT count(*) FROM app_dependencies WHERE lib_id=?1)+(SELECT count(*) FROM lib_dependencies WHERE dependency_id=?1)",[id],|row|row.get(0))?;
        if dependents > 0 {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "lib is required by an installed App/Lib",
            )));
        }
    }
    let root = match child_dir(&parent, std::ffi::OsStr::new(kind)) {
        Ok(root) => root,

        Err(e) => return Err(e),
    };
    flock(&root, FlockOperation::LockExclusive).map_err(std::io::Error::from)?;
    check_lib_root(profile, &parent, &root, kind)?;
    match resource_dir(&root, id) {
        Ok(dir) => {
            check_component_target(&dir, kind)?;
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
            let tx =
                database.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let table = if kind == "app" { "apps" } else { "libs" };
            tx.execute(&format!("DELETE FROM {table} WHERE id=?1"), [id])?;
            renameat(&target_parent, leaf, &stage, "artifact").map_err(layout_error)?;
            let rollback = scopeguard::guard((), |_| {
                let _ = renameat(&stage, "artifact", &target_parent, leaf);
                let _ = target_parent.sync_all();
            });
            target_parent.sync_all()?;
            root.sync_all()?;
            stage.sync_all()?;
            parent.sync_all()?;
            tx.commit()?;
            scopeguard::ScopeGuard::into_inner(rollback);
            drop(cleanup);
            parent.sync_all()?;
            Ok(())
        }
        Err(error) => Err(error),
    }
}

/// Read validated immutable frontend resources using the same no-follow traversal.
pub(crate) fn installed_apps(
    profile: &IdentityProfile,
    id: Option<&str>,
) -> Result<std::collections::HashMap<String, webc::Container>> {
    if let Some(id) = id {
        valid_id(id)?;
    }
    let root = match child_dir(&profile_dir(profile)?, std::ffi::OsStr::new("app")) {
        Ok(root) => root,
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            let conn = installation_database(profile, false)?;
            if registered_packages(&conn, "app", id)?.is_empty() {
                return Ok(std::collections::HashMap::new());
            }
            return Err(Error::Io(e));
        }
        Err(e) => return Err(e),
    };
    rustix::fs::flock(&root, rustix::fs::FlockOperation::LockShared)
        .map_err(std::io::Error::from)?;
    let conn = installation_database(profile, false)?;
    let ids = registered_packages(&conn, "app", id)?;
    let mut apps = std::collections::HashMap::new();
    for (id, version) in ids {
        let package = read_package(&artifact_bytes(&root, &id, "app")?)?;
        let api = package_metadata(&package)?;
        check_resource_identity(&api, &id)?;
        if api["kind"] != "app" {
            return Err(Error::InvalidComponent("expected a frontend app".into()));
        }
        if api["version"].as_str().unwrap() != version {
            return Err(Error::InvalidComponent(format!(
                "registered version differs from file for {id}"
            )));
        }
        check_registration_dependencies(&conn, &id, "app", &api)?;
        apps.insert(id, package);
    }
    Ok(apps)
}
