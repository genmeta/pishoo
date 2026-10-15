//! Lib files: directory traversal, queries, atomic installation and removal.
//! Opened directory handles refuse symlinks throughout traversal.

use std::os::unix::fs::MetadataExt;

use dhttp_home::identity::IdentityProfile;

use super::{WasmRuntime, lib_metadata, package_metadata, read_package};
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
    child_dir(&profile_dir(profile)?, std::ffi::OsStr::new("libs"))
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
    let parent = resource_parent(root, id, false)?;
    child_dir(
        &parent,
        std::ffi::OsStr::new(id.rsplit('/').next().unwrap()),
    )
}

// Called under the root's exclusive commit lock. The returned handle is the
// actual target parent; no unchecked slash-containing path reaches openat.
fn resource_parent(root: &std::fs::File, id: &str, create: bool) -> Result<std::fs::File> {
    valid_id(id)?;
    // Placement follows the signed publisher identity. It grants no privileges.
    if id
        .split_once('/')
        .is_some_and(|(publisher, _)| publisher == "std.dhttp.net")
    {
        if create {
            match rustix::fs::mkdirat(root, "std", rustix::fs::Mode::from_raw_mode(0o700)) {
                Ok(()) => root.sync_all()?,
                Err(rustix::io::Errno::EXIST) => {}
                Err(e) => return Err(layout_error(e)),
            }
        }
        child_dir(root, std::ffi::OsStr::new("std"))
    } else {
        Ok(root.try_clone()?)
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
    let previous = match expanded_file(dir, ".webc-proof") {
        Ok(_) => expanded_bytes(dir)?,
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            let file = match artifact_file(dir, kind) {
                Ok(file) => file,
                Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(());
                }
                Err(error) => return Err(error),
            };
            let mut previous = Vec::new();
            file.take(64 * 1024 * 1024 + 1).read_to_end(&mut previous)?;
            previous
        }
        Err(e) => return Err(e),
    };
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
        if id == "std" {
            continue;
        }
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

// Installation facts belong to config.db; startup never discovers files as an
// implicit installation. Read-only queries do not audit the database graph.
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
        let ready: bool = conn.query_row("SELECT count(*)=6 FROM sqlite_master WHERE type='table' AND name IN ('libs','apps','app_dependencies','lib_dependencies','lib_capabilities','lib_capability_operations')",[],|row|row.get(0))?;
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

// All values are derived from the authenticated container; no runtime registry.
fn capability_rows(
    package: &webc::Container,
) -> Result<Vec<(String, String, Vec<(String, String)>)>> {
    let document: serde_json::Value =
        serde_json::from_slice(&super::package_file(package, "/metadata/openapi.json")?)
            .map_err(|e| Error::InvalidComponent(e.to_string()))?;
    let mut rows = Vec::new();
    if let Some(groups) = document["x-pishoo"]["access-groups"].as_object() {
        for (id, group) in groups {
            let mut endpoints = Vec::new();
            for (path, item) in document["paths"].as_object().unwrap() {
                for method in [
                    "get", "head", "post", "put", "patch", "delete", "options", "trace",
                ] {
                    if item[method]["x-pishoo"]["access-group"] == *id {
                        endpoints.push((method.to_ascii_uppercase(), format!("/std{path}")));
                    }
                }
            }
            endpoints.sort();
            rows.push((
                id.clone(),
                group["title"].as_str().unwrap().to_owned(),
                endpoints,
            ));
        }
    }
    rows.sort();
    Ok(rows)
}

fn package_routes(
    package: &webc::Container,
    id: &str,
    kind: &str,
) -> Result<std::collections::BTreeMap<String, Vec<String>>> {
    let mut routes = std::collections::BTreeMap::new();
    if kind == "lib" {
        let api =
            super::validate_openapi(&super::package_file(package, "/metadata/openapi.json")?)?;
        for (path, item) in api.paths.as_ref().unwrap() {
            routes.insert(
                format!("/std{path}"),
                item.methods()
                    .into_iter()
                    .map(|(m, _)| m.as_str().to_owned())
                    .collect(),
            );
        }
    } else {
        let mappings = package.manifest().filesystem().unwrap().unwrap();
        let mapping = &mappings[0];
        let volume = package.get_volume(&mapping.volume_name).unwrap();
        let root = mapping
            .host_path
            .as_deref()
            .unwrap_or("/")
            .trim_end_matches('/');
        let mut pending = vec![format!("{root}/")];
        while let Some(dir) = pending.pop() {
            for (name, _, metadata) in volume.read_dir(dir.as_str()).unwrap() {
                let child = format!("{}{name}", dir);
                if matches!(metadata, webc::Metadata::Dir { .. }) {
                    pending.push(format!("{child}/"));
                } else {
                    let suffix = child.strip_prefix(root).unwrap();
                    let suffix = if suffix == "/index.html" { "/" } else { suffix };
                    let path = format!("/std/{}{suffix}", id.rsplit('/').next().unwrap());
                    routes.insert(path, vec!["GET".into(), "HEAD".into()]);
                }
            }
        }
        // The canonical root's redirect is also an actual static operation.
        routes.insert(
            format!("/std/{}", id.rsplit('/').next().unwrap()),
            vec!["GET".into(), "HEAD".into()],
        );
    }
    Ok(routes)
}

fn check_routes(
    profile: &IdentityProfile,
    conn: &rusqlite::Connection,
    id: &str,
    kind: &str,
    package: &webc::Container,
) -> Result<()> {
    let routes = package_routes(package, id, kind)?;
    for other_kind in ["lib", "app"] {
        for (other, version) in registered_packages(conn, other_kind, None)? {
            if other_kind == kind && other == id {
                continue;
            }
            let root = child_dir(
                &profile_dir(profile)?,
                std::ffi::OsStr::new(&format!("{other_kind}s")),
            )?;
            let other_package = read_package(&artifact_bytes(&root, &other, other_kind)?)?;
            let metadata = package_metadata(&other_package)?;
            check_resource_identity(&metadata, &other)?;
            if metadata["version"] != version {
                return Err(Error::InvalidConfig(format!(
                    "registered version differs from file for {other}"
                )));
            }
            for (path, methods) in package_routes(&other_package, &other, other_kind)? {
                if let Some(ours) = routes.get(&path)
                    && ((kind == "lib" && other_kind == "lib")
                        || methods.iter().any(|m| ours.contains(m)))
                {
                    return Err(Error::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("route {path} conflicts with installed {other_kind} {other}"),
                    )));
                }
            }
        }
    }
    Ok(())
}

// ACL is path based, so a newly installed provider must not silently inherit
// existing rules for its operations. Explicit rule cleanup precedes takeover.
fn check_existing_rules(
    profile: &IdentityProfile,
    conn: &rusqlite::Connection,
    id: &str,
    kind: &str,
    package: &webc::Container,
) -> Result<()> {
    let old = registered_packages(conn, kind, Some(id));
    let previous = match old {
        Ok(_) => {
            let root = child_dir(
                &profile_dir(profile)?,
                std::ffi::OsStr::new(&format!("{kind}s")),
            )?;
            package_routes(&read_package(&artifact_bytes(&root, id, kind)?)?, id, kind)?
        }
        Err(Error::RouteNotFound) => std::collections::BTreeMap::new(),
        Err(e) => return Err(e),
    };
    let path = profile.join("db/access.db");
    if !path.try_exists()? {
        return Ok(());
    }
    if !path.symlink_metadata()?.is_file() {
        return Err(Error::InvalidConfig(
            "access.db must be a regular file".into(),
        ));
    }
    let access =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    for (path, methods) in package_routes(package, id, kind)? {
        for method in methods {
            if previous.get(&path).is_some_and(|m| m.contains(&method)) {
                continue;
            }
            // These exact public discovery grants are installed before the
            // standard packages. Custom rules and other publishers still need
            // explicit review; no write operation inherits this exception.
            let public_default = match (id, kind, method.as_str(), path.trim_end_matches('/')) {
                (
                    "std.dhttp.net/profile",
                    "lib",
                    "GET",
                    "/std/profile" | "/std/profile/avatar" | "/std/profile/context",
                ) => "/std/profile",
                ("std.dhttp.net/profile", "lib", "HEAD", "/std/profile/context") => {
                    "/std/profile/context"
                }
                ("std.dhttp.net/workspace", "app", "GET" | "HEAD", route)
                    if route == "/std/workspace" || route.starts_with("/std/workspace/") =>
                {
                    "/std/workspace"
                }
                _ => "",
            };
            let exists: bool = access.query_row(
                "SELECT EXISTS(SELECT 1 FROM access_rules WHERE (api='/' OR api=?1 OR substr(?1,1,length(api)+1)=api||'/') AND method IN (?2,'*') AND NOT (api=?3 AND method=?2 AND effect='allow' AND grantee_type=4 AND grantee='*?'))",
                rusqlite::params![path.trim_end_matches('/'), method, public_default],
                |r| r.get(0),
            )?;
            if exists {
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "existing ACL rules for {method} {path}; explicitly remove/review these rules before installing a new provider"
                    ),
                )));
            }
        }
    }
    Ok(())
}

fn save_registration(
    conn: &rusqlite::Connection,
    id: &str,
    kind: &str,
    api: &serde_json::Value,
    package: &webc::Container,
) -> Result<()> {
    check_name(conn, id, kind)?;
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

    if kind == "lib" {
        conn.execute("DELETE FROM lib_capabilities WHERE lib_id=?1", [id])?;
        for (capability, title, endpoints) in capability_rows(package)? {
            conn.execute(
                "INSERT INTO lib_capabilities(lib_id,capability_id,title) VALUES(?1,?2,?3)",
                rusqlite::params![id, capability, title],
            )?;
            for (method, path) in endpoints {
                conn.execute("INSERT INTO lib_capability_operations(lib_id,capability_id,method,path) VALUES(?1,?2,?3,?4)",rusqlite::params![id,capability,method,path])?;
            }
        }
    }
    let broken: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_foreign_key_check)",
        [],
        |row| row.get(0),
    )?;
    if broken {
        return Err(Error::InvalidConfig(
            "installation would leave broken package/dependency/capability references".into(),
        ));
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

fn expanded_error() -> Error {
    Error::InvalidComponent("invalid or modified expanded package".into())
}

fn expanded_file(dir: &std::fs::File, path: &str) -> Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags, openat};
    let mut parent = dir.try_clone()?;
    let mut parts = path.split('/').peekable();
    while let Some(part) = parts.next() {
        if part.is_empty() || matches!(part, "." | "..") || part.contains('\\') {
            return Err(expanded_error());
        }
        if parts.peek().is_some() {
            parent = child_dir(&parent, std::ffi::OsStr::new(part))?;
        } else {
            let file = std::fs::File::from(
                openat(
                    &parent,
                    part,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(layout_error)?,
            );
            if !file.metadata()?.is_file() {
                return Err(expanded_error());
            }
            return Ok(file);
        }
    }
    Err(expanded_error())
}

// Keep the original signed binary headers, but store all file/atom payloads
// separately. These transient spans are not installation facts or runtime state.
fn expanded_layout(bytes: &[u8]) -> Result<Vec<(String, webc::v3::Span)>> {
    let package = read_package(bytes)?;
    let metadata = package_metadata(&package)?;
    let mappings = package
        .manifest()
        .filesystem()
        .map_err(|_| expanded_error())?
        .unwrap();
    let mapping = &mappings[0];
    let mapped_root = mapping
        .host_path
        .as_deref()
        .unwrap_or("/")
        .trim_end_matches('/');
    let reader =
        webc::v3::read::OwnedReader::parse(bytes.to_vec()).map_err(|_| expanded_error())?;
    let index = reader.index();
    let atom = &index.atoms;
    let section = webc::v3::read::Section::parse(
        3,
        Some(
            bytes[atom.span.start + 1..atom.span.start + 33]
                .try_into()
                .unwrap(),
        ),
        bytes[atom.span.start + 41..atom.span.end()].to_vec().into(),
    )
    .map_err(|_| expanded_error())?;
    let mut files = Vec::new();
    for entry in section.as_atoms().unwrap().iter_with_offsets() {
        let (_, _, _, span) = entry.map_err(|_| expanded_error())?;
        files.push((
            format!("{}.wasm", metadata["name"].as_str().unwrap()),
            span.with_offset(atom.span.start),
        ));
    }
    for entry in reader.iter_volumes() {
        let (volume_name, volume) = entry.map_err(|_| expanded_error())?;
        let offset = index.volumes[volume_name].span.start;
        let mut pending = vec![(String::new(), volume.root().map_err(|_| expanded_error())?)];
        while let Some((parent, directory)) = pending.pop() {
            for entry in directory.entries() {
                let (name, _, entry) = entry.map_err(|_| expanded_error())?;
                let path = format!("{parent}/{name}");
                match entry {
                    webc::v3::read::DirEntry::Dir(dir) => pending.push((path, dir)),
                    webc::v3::read::DirEntry::File(file) => {
                        let destination = if volume_name == "pishoo-signature" {
                            "signature.json".to_owned()
                        } else if volume_name == mapping.volume_name
                            && path
                                .strip_prefix(mapped_root)
                                .is_some_and(|tail| tail.starts_with('/'))
                        {
                            let relative = path
                                .strip_prefix(mapped_root)
                                .unwrap()
                                .trim_start_matches('/');
                            if metadata["kind"] == "app" {
                                format!("public/{relative}")
                            } else if relative == "openapi.json" {
                                relative.to_owned()
                            } else {
                                format!("metadata/{relative}")
                            }
                        } else {
                            // Preserve signed files outside the logical mount too.
                            format!(
                                ".volumes/{}{path}",
                                volume_name
                                    .as_bytes()
                                    .iter()
                                    .map(|b| format!("{b:02x}"))
                                    .collect::<String>()
                            )
                        };
                        files.push((destination, file.span().with_offset(offset)));
                    }
                    _ => return Err(expanded_error()),
                }
            }
        }
    }
    files.sort_by_key(|(_, span)| (span.start, span.len));
    let mut position = 0;
    let mut paths = std::collections::HashSet::new();
    for (path, span) in &files {
        if path
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | "..") || part.contains('\\'))
            || !paths.insert(path)
            || span.start < position
            || span.end() > bytes.len()
        {
            return Err(expanded_error());
        }
        position = span.end();
    }
    Ok(files)
}

fn write_expanded(dir: &std::fs::File, bytes: &[u8]) -> Result<()> {
    use rustix::fs::{Mode, mkdirat};
    let files = expanded_layout(bytes)?;
    let header = serde_json::to_vec(&(bytes.len(), &files)).map_err(|_| expanded_error())?;
    let mut proof = b"pishoo-expanded-webc-v1\0".to_vec();
    proof.extend_from_slice(&(header.len() as u64).to_le_bytes());
    proof.extend_from_slice(&header);
    let mut position = 0;
    for (path, span) in files {
        proof.extend_from_slice(&bytes[position..span.start]);
        position = span.end();
        let mut parent = dir.try_clone()?;
        let mut parts = path.split('/').peekable();
        while let Some(part) = parts.next() {
            if parts.peek().is_some() {
                match mkdirat(&parent, part, Mode::from_raw_mode(0o700)) {
                    Ok(()) => parent.sync_all()?,
                    Err(rustix::io::Errno::EXIST) => {}
                    Err(e) => return Err(layout_error(e)),
                }
                parent = child_dir(&parent, std::ffi::OsStr::new(part))?;
            } else {
                write_component(&parent, part, &bytes[span.start..span.end()])?;
            }
        }
    }
    proof.extend_from_slice(&bytes[position..]);
    write_component(dir, ".webc-proof", &proof)?;
    let package = read_package(bytes)?;
    write_component(
        dir,
        "manifest.json",
        &serde_json::to_vec_pretty(package.manifest()).map_err(|_| expanded_error())?,
    )?;
    dir.sync_all()?;
    Ok(())
}

fn expanded_bytes(dir: &std::fs::File) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut proof = Vec::new();
    expanded_file(dir, ".webc-proof")?
        .take(80 * 1024 * 1024 + 1)
        .read_to_end(&mut proof)?;
    let magic = b"pishoo-expanded-webc-v1\0";
    if proof.len() > 80 * 1024 * 1024 || !proof.starts_with(magic) {
        return Err(expanded_error());
    }
    let raw = proof
        .get(magic.len()..magic.len() + 8)
        .ok_or_else(expanded_error)?;
    let header_len = usize::try_from(u64::from_le_bytes(raw.try_into().unwrap()))
        .map_err(|_| expanded_error())?;
    if header_len > 16 * 1024 * 1024 {
        return Err(expanded_error());
    }
    let header_start = magic.len() + 8;
    let header_end = header_start
        .checked_add(header_len)
        .ok_or_else(expanded_error)?;
    let (length, files): (usize, Vec<(String, webc::v3::Span)>) = serde_json::from_slice(
        proof
            .get(header_start..header_end)
            .ok_or_else(expanded_error)?,
    )
    .map_err(|_| expanded_error())?;
    if length > 64 * 1024 * 1024 || files.len() > 10001 {
        return Err(expanded_error());
    }
    let mut bytes = Vec::with_capacity(length);
    let mut cursor = header_end;
    for (path, span) in &files {
        let end = span
            .start
            .checked_add(span.len)
            .filter(|end| *end <= length)
            .ok_or_else(expanded_error)?;
        let gap = span
            .start
            .checked_sub(bytes.len())
            .ok_or_else(expanded_error)?;
        let stop = cursor.checked_add(gap).ok_or_else(expanded_error)?;
        bytes.extend_from_slice(proof.get(cursor..stop).ok_or_else(expanded_error)?);
        cursor = stop;
        let before = bytes.len();
        expanded_file(dir, path)?
            .take(span.len as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() != end || bytes.len() - before != span.len {
            return Err(expanded_error());
        }
    }
    let remaining = length.checked_sub(bytes.len()).ok_or_else(expanded_error)?;
    if proof.len().checked_sub(cursor) != Some(remaining) {
        return Err(expanded_error());
    }
    bytes.extend_from_slice(&proof[cursor..]);
    // Installation authenticated the package and wrote this layout. Reading it
    // only reconstructs the container; it does not rehash or reverify payloads.
    // Bounds and no-follow traversal above are necessary to read files safely.
    Ok(bytes)
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

// Registered previous WEBC installations remain readable until an explicit
// reinstall. No startup relocation, package discovery, or data migration occurs.
fn legacy_parent(root: &std::fs::File, id: &str, kind: &str) -> Result<std::fs::File> {
    let identity = child_dir(root, std::ffi::OsStr::new(".."))?;
    let legacy = child_dir(&identity, std::ffi::OsStr::new(kind))?;
    match id.split_once('/') {
        Some((publisher, _)) => child_dir(&legacy, std::ffi::OsStr::new(publisher)),
        None => Ok(legacy),
    }
}

fn artifact_bytes(root: &std::fs::File, id: &str, kind: &str) -> Result<Vec<u8>> {
    use std::io::Read;
    valid_id(id)?;
    let dir = match resource_dir(root, id) {
        Ok(dir) => dir,
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => child_dir(
            &legacy_parent(root, id, kind)?,
            std::ffi::OsStr::new(id.rsplit('/').next().unwrap()),
        )?,
        Err(e) => return Err(e),
    };
    match expanded_file(&dir, ".webc-proof") {
        Ok(_) => return expanded_bytes(&dir),
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
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
            let package = webc::Container::from_bytes_and_version(bytes.into(), webc::Version::V3)
                .map_err(|error| Error::InvalidComponent(error.to_string()))?;
            package_metadata(&package)?;
            Ok(package)
        }) {
            Ok(package) => {
                let mut value = lib_metadata(&package, Some(&registered));
                value["version"] = version.into();
                value
            }
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
        &http::Uri::from_static("/std/sys/settings"),
        None,
    )?;
    Ok(dir)
}

fn check_component_target(dir: &std::fs::File, kind: &str) -> Result<()> {
    match expanded_file(dir, ".webc-proof") {
        Ok(_) => return Ok(()),
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
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
    let current_root = child_dir(parent, std::ffi::OsStr::new(&format!("{kind}s")))?;
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
    use rustix::fs::{FlockOperation, Mode, flock, mkdirat, renameat};
    valid_id(id)?;
    if id.rsplit('/').next() == Some("std") {
        return Err(Error::BadRequest(
            "std is reserved for standard package directories".into(),
        ));
    }
    if kind == "app" && !id.contains('/') {
        return Err(Error::BadRequest(
            "frontend app requires publisher/name ID".into(),
        ));
    }
    let parent = initialized_profile(profile)?;
    flock(&parent, FlockOperation::LockExclusive).map_err(std::io::Error::from)?;
    {
        match mkdirat(&parent, format!("{kind}s"), Mode::from_raw_mode(0o700)) {
            Ok(()) => parent.sync_all()?,
            Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(layout_error(error)),
        }
    }
    let root = child_dir(&parent, std::ffi::OsStr::new(&format!("{kind}s")))?;
    // Layout inspection is only part of an explicit install, never discovery.
    flock(&root, FlockOperation::LockShared).map_err(std::io::Error::from)?;
    disk_ids(&root)?;
    let mut database = installation_database(profile, true)?;
    check_name(&database, id, kind)?;
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
        runtime.compile(
            &package
                .get_atom("backend")
                .ok_or_else(|| Error::InvalidComponent("expected a backend lib".into()))?,
        )?;
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
    // Prepare a complete immutable directory before taking the publication lock.
    // All readers use this root lock; the identity lock serializes management.
    let name = temporary_name()?;
    mkdirat(&parent, &name, Mode::from_raw_mode(0o700)).map_err(layout_error)?;
    let cleanup_dir = cap_std::fs::Dir::from_std_file(parent.try_clone()?);
    let cleanup = scopeguard::guard((), |_| {
        let _ = cleanup_dir.remove_dir_all(&name);
    });
    let stage = child_dir(&parent, std::ffi::OsStr::new(&name))?;
    write_expanded(&stage, bytes)?;
    flock(&root, FlockOperation::LockExclusive).map_err(std::io::Error::from)?;
    check_lib_root(profile, &parent, &root, kind)?;
    check_name(&database, id, kind)?;
    let target_parent = resource_parent(&root, id, true)?;
    if target_parent.metadata()?.dev() != parent.metadata()?.dev() {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "package directory must share the identity filesystem",
        )));
    }
    let leaf = id.rsplit('/').next().unwrap();
    let previous = match resource_dir(&root, id) {
        Ok(dir) => {
            check_component_target(&dir, kind)?;
            check_publisher_update(&dir, id, bytes, kind)?;
            Some(dir)
        }
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    if previous.is_none() && registered_packages(&database, kind, Some(id)).is_ok() {
        let old = artifact_bytes(&root, id, kind)?;
        if signed_owner(&old)? != signed_owner(bytes)? {
            return Err(Error::InvalidComponent(
                "publisher owner_hash changed; ordinary update is refused".into(),
            ));
        }
    }
    let tx = database.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    check_routes(profile, &tx, id, kind, &package)?;
    check_existing_rules(profile, &tx, id, kind, &package)?;
    save_registration(&tx, id, kind, &metadata, &package)?;
    if previous.is_some() {
        renameat(&target_parent, leaf, &stage, ".previous").map_err(layout_error)?;
    }
    let restore = scopeguard::guard((), |_| {
        if previous.is_some() {
            let _ = renameat(&stage, ".previous", &target_parent, leaf);
            let _ = target_parent.sync_all();
        }
    });
    stage.sync_all()?;
    renameat(&parent, &name, &target_parent, leaf).map_err(layout_error)?;
    let rollback = scopeguard::guard((), |_| {
        let _ = renameat(&target_parent, leaf, &parent, &name);
        let _ = target_parent.sync_all();
        let _ = parent.sync_all();
    });
    target_parent.sync_all()?;
    parent.sync_all()?;
    tx.commit()?;
    scopeguard::ScopeGuard::into_inner(rollback);
    scopeguard::ScopeGuard::into_inner(restore);
    if previous.is_some() {
        let published = cap_std::fs::Dir::from_std_file(stage);
        let _ = published.remove_dir_all(".previous");
        let _ = published.into_std_file().sync_all();
    }
    drop(cleanup);
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
        let dependents = database
            .prepare(
                "SELECT 'app ' || app_id FROM app_dependencies WHERE lib_id=?1 \
                 UNION SELECT 'lib ' || lib_id FROM lib_dependencies WHERE dependency_id=?1 ORDER BY 1",
            )?
            .query_map([id], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if !dependents.is_empty() {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "lib is required by an installed App/Lib: {}; remove these packages first",
                    dependents.join(", "),
                ),
            )));
        }
    }
    let root = match child_dir(&parent, std::ffi::OsStr::new(&format!("{kind}s"))) {
        Ok(root) => root,

        Err(e) => return Err(e),
    };
    flock(&root, FlockOperation::LockExclusive).map_err(std::io::Error::from)?;
    check_lib_root(profile, &parent, &root, kind)?;
    let target_parent = match resource_dir(&root, id) {
        Ok(_) => resource_parent(&root, id, false)?,
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            legacy_parent(&root, id, kind)?
        }
        Err(e) => return Err(e),
    };
    match child_dir(
        &target_parent,
        std::ffi::OsStr::new(id.rsplit('/').next().unwrap()),
    ) {
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

/// Read installed frontend resources without repeating installation validation.
pub(crate) fn installed_apps(
    profile: &IdentityProfile,
    id: Option<&str>,
) -> Result<std::collections::HashMap<String, webc::Container>> {
    if let Some(id) = id {
        valid_id(id)?;
    }
    let root = match child_dir(&profile_dir(profile)?, std::ffi::OsStr::new("apps")) {
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
    for (id, _) in ids {
        let package = webc::Container::from_bytes_and_version(
            artifact_bytes(&root, &id, "app")?.into(),
            webc::Version::V3,
        )
        .map_err(|error| Error::InvalidComponent(error.to_string()))?;
        package_metadata(&package)?;
        apps.insert(id, package);
    }
    Ok(apps)
}
