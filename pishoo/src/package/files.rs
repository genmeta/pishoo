//! Package files: directory traversal, queries, atomic installation and removal.
//! Opened directory handles refuse symlinks throughout traversal.

use std::os::unix::fs::MetadataExt;

use dhttp_home::identity::IdentityProfile;

use super::{WasmRuntime, lib_metadata, package_metadata, read_package};
use crate::{Error, Result};

#[cfg(test)]
#[path = "../../tests/unit/package/capabilities.rs"]
mod tests;

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

// The system service installs on behalf of the identity's filesystem owner.
// Use opened handles so symlink replacement cannot redirect ownership changes.
fn inherit_owner(file: &std::fs::File, parent: &std::fs::File) -> Result<()> {
    if rustix::process::geteuid().is_root() {
        let owner = parent.metadata()?;
        let current = file.metadata()?;
        if current.dev() != owner.dev() {
            return Ok(());
        }
        if current.uid() != owner.uid() || current.gid() != owner.gid() {
            rustix::fs::fchown(
                file,
                Some(rustix::fs::Uid::from_raw(owner.uid())),
                Some(rustix::fs::Gid::from_raw(owner.gid())),
            )
            .map_err(std::io::Error::from)?;
            // Linux may clear set-ID bits when changing ownership. Existing
            // filesystem modes are independent facts and must remain intact.
            if file.metadata()?.mode() & 0o7777 != current.mode() & 0o7777 {
                rustix::fs::fchmod(
                    file,
                    rustix::fs::Mode::from_raw_mode((current.mode() & 0o7777) as _),
                )
                .map_err(std::io::Error::from)?;
            }
        }
    }
    Ok(())
}

fn database_owner(profile: &IdentityProfile) -> Result<()> {
    if rustix::process::geteuid().is_root() {
        let identity = profile_dir(profile)?;
        let owner = identity.metadata()?;
        let mut pending = vec![child_dir(&identity, std::ffi::OsStr::new(".volume"))?];
        while let Some(dir) = pending.pop() {
            inherit_owner(&dir, &identity)?;
            let entries = cap_std::fs::Dir::from_std_file(dir.try_clone()?);
            for entry in entries.entries()? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    pending.push(child_dir(&dir, &entry.file_name())?);
                } else if entry.file_type()?.is_file() {
                    let file = expanded_file(
                        &dir,
                        entry.file_name().to_str().ok_or_else(expanded_error)?,
                    )?;
                    if file.metadata()?.nlink() != 1 {
                        return Err(expanded_error());
                    }
                    inherit_owner(&file, &identity)?;
                }
            }
        }
        let _ = owner;
    }
    Ok(())
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

pub(crate) fn lib_root(profile: &IdentityProfile) -> Result<std::fs::File> {
    child_dir(&profile_dir(profile)?, std::ffi::OsStr::new("lib"))
}

pub(crate) fn valid_id(id: &str) -> Result<()> {
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
        let directory = child_dir(root, std::ffi::OsStr::new("std"))?;
        if create {
            inherit_owner(&directory, root)?;
        }
        Ok(directory)
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

// Installation facts belong to config.db; startup never discovers files as an
// implicit installation. Read-only queries do not audit the database graph.
fn installation_database(profile: &IdentityProfile, create: bool) -> Result<rusqlite::Connection> {
    if create {
        crate::setup::load_server_config(profile)?;
    }
    let mut conn = crate::setup::open_config(
        profile,
        if create {
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
        } else {
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
        },
    )?;
    if create {
        for schema in ["main", "pkg_registry", "acl"] {
            conn.execute_batch(&format!(
                "PRAGMA {schema}.journal_mode=DELETE; PRAGMA {schema}.synchronous=FULL;"
            ))?;
        }
        migrate_registration(&mut conn)?;
        database_owner(profile)?;
    }
    Ok(conn)
}

// External identities remain publisher/name; same-kind names are the natural
// database keys. Legacy short aliases keep a NULL author without fabrication.
fn package_identity(conn: &rusqlite::Connection, table: &str) -> Result<&'static str> {
    let author: bool = conn.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM pragma_table_info('{table}') WHERE name='author')"),
        [],
        |row| row.get(0),
    )?;
    Ok(if author {
        "CASE WHEN author IS NULL THEN name ELSE author||'/'||name END"
    } else {
        "id"
    })
}

fn migrate_registration(conn: &mut rusqlite::Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let operation_sql: String = tx.query_row(
        "SELECT sql FROM acl.sqlite_master WHERE name='lib_capability_operation'",
        [],
        |r| r.get(0),
    )?;
    if operation_sql.contains("'/std/'") {
        tx.execute_batch(
            "ALTER TABLE acl.lib_capability_operation RENAME TO pishoo_legacy_operation",
        )?;
        tx.execute_batch(include_str!("packages.sql"))?;
        tx.execute_batch("INSERT INTO acl.lib_capability_operation SELECT * FROM acl.pishoo_legacy_operation; DROP TABLE acl.pishoo_legacy_operation;")?;
    }

    tx.execute("UPDATE lib_capability_operation SET path='/'||lib_name||substr(path,5) WHERE substr(path,1,5)='/std/' AND lib_name IN (SELECT name FROM package WHERE has_lib=1 AND (author IS NULL OR author<>'std.dhttp.net'))",[])?;
    for schema in ["pkg_registry", "acl"] {
        if tx
            .prepare(&format!("PRAGMA {schema}.foreign_key_check"))?
            .exists([])?
        {
            return Err(Error::InvalidConfig(
                "broken installation references".into(),
            ));
        }
    }
    let broken: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM package_dependency d JOIN package p ON p.name=d.dependency_name WHERE p.version<>d.version)", [], |r|r.get(0))?;
    if broken {
        return Err(Error::InvalidConfig(
            "broken exact package dependencies".into(),
        ));
    }
    tx.commit()?;
    Ok(())
}

fn registered_packages(
    conn: &rusqlite::Connection,
    kind: &str,
    id: Option<&str>,
) -> Result<Vec<(String, String)>> {
    let table = match kind {
        "app" => "app",
        "lib" => "lib",
        _ => "package",
    };
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_list WHERE type IN ('table','view') AND name=?1)",
        [table],
        |row| row.get(0),
    )?;
    let mut records = Vec::new();
    if exists {
        let identity = package_identity(conn, table)?;
        let mut stmt = conn.prepare(&format!("SELECT {identity},name,version FROM {table} WHERE ?1 IS NULL OR ({identity})=?1 ORDER BY ({identity})"))?;
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
    let table = match kind {
        "app" => "app",
        "lib" => "lib",
        _ => "package",
    };
    let identity = package_identity(conn, table)?;
    let other: Option<String> = conn
        .query_row(
            &format!("SELECT {identity} FROM {table} WHERE name=?1 AND ({identity})<>?2"),
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
    // Installation validation rejects duplicate modern/legacy declarations.
    if let Some(groups) = document
        .get("x-access-groups")
        .or_else(|| document["x-pishoo"].get("access-groups"))
        .and_then(serde_json::Value::as_object)
    {
        for (id, group) in groups {
            let mut endpoints = Vec::new();
            for (path, item) in document["paths"].as_object().unwrap() {
                for method in [
                    "get", "head", "post", "put", "patch", "delete", "options", "trace",
                ] {
                    let group = item[method]
                        .get("x-access-group")
                        .or_else(|| item[method]["x-pishoo"].get("access-group"));
                    if group.and_then(serde_json::Value::as_str) == Some(id.as_str()) {
                        endpoints.push((method.to_ascii_uppercase(), path.clone()));
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
        let prefix = if id
            .split_once('/')
            .is_some_and(|(publisher, _)| publisher == "std.dhttp.net")
        {
            "/std".to_owned()
        } else {
            format!("/{}", id.rsplit('/').next().unwrap())
        };
        let api =
            super::validate_openapi(&super::package_file(package, "/metadata/openapi.json")?)?;
        for (path, item) in api.paths.as_ref().unwrap() {
            routes.insert(
                format!("{prefix}{path}"),
                item.methods()
                    .into_iter()
                    .map(|(m, _)| m.as_str().to_owned())
                    .collect(),
            );
        }
    } else {
        let mappings = package.manifest().filesystem().unwrap().unwrap();
        let mapping = mappings
            .iter()
            .find(|m| m.mount_path == "/public")
            .ok_or_else(expanded_error)?;
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
                    let path = format!("/app/{}{suffix}", id.rsplit('/').next().unwrap());
                    routes.insert(path, vec!["GET".into(), "HEAD".into()]);
                }
            }
        }
        // The canonical root's redirect is also an actual static operation.
        routes.insert(
            format!("/app/{}", id.rsplit('/').next().unwrap()),
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
    if kind == "app" || !id.starts_with("std.dhttp.net/") {
        let mount = if kind == "app" {
            format!("/app/{}", id.rsplit('/').next().unwrap())
        } else {
            format!("/{}", id.rsplit('/').next().unwrap())
        };
        let locations = conn
            .prepare("SELECT location FROM proxy_location")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for location in locations {
            let path = location
                .strip_prefix("= ")
                .unwrap_or(&location)
                .trim_end_matches('/');
            if path == mount
                || path
                    .strip_prefix(&mount)
                    .is_some_and(|tail| tail.starts_with('/'))
            {
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("{kind} {mount} conflicts with configured proxy {location}"),
                )));
            }
        }
    }
    let routes = package_routes(package, id, kind)?;
    for other_kind in ["lib", "app"] {
        for (other, version) in registered_packages(conn, other_kind, None)? {
            if other == id {
                continue;
            }
            let root = child_dir(&profile_dir(profile)?, std::ffi::OsStr::new(other_kind))?;
            let other_package = webc::Container::from_bytes_and_version(
                installed_bytes(profile, &root, &other, other_kind)?.into(),
                webc::Version::V3,
            )
            .map_err(|_| expanded_error())?;
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
    same_version: bool,
) -> Result<()> {
    if same_version {
        return Ok(());
    }

    let old = registered_packages(conn, kind, Some(id));
    let previous = match old {
        Ok(_) => {
            let root = child_dir(
                &profile_dir(profile)?,
                std::ffi::OsStr::new(&kind.to_string()),
            )?;
            let previous = webc::Container::from_bytes_and_version(
                installed_bytes(profile, &root, id, kind)?.into(),
                webc::Version::V3,
            )
            .map_err(|_| expanded_error())?;
            package_routes(&previous, id, kind)?
        }
        Err(Error::RouteNotFound) => std::collections::BTreeMap::new(),
        Err(e) => return Err(e),
    };
    let path = profile.join(".volume/sys/access/database");
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
    let rules: bool = access.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='access_rules')",
        [],
        |r| r.get(0),
    )?;
    if !rules {
        return Ok(());
    }
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
                    if route == "/app/workspace" || route.starts_with("/app/workspace/") =>
                {
                    "/app/workspace"
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
    api: &serde_json::Value,
    package: &webc::Container,
) -> Result<()> {
    check_name(conn, id, "pkg")?;
    let name = id.rsplit('/').next().unwrap();
    let author = id.split_once('/').map(|(author, _)| author);
    let incompatible: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM package_dependency WHERE dependency_name=?1 AND version<>?2)",
        rusqlite::params![name, api["version"].as_str().unwrap()],
        |r| r.get(0),
    )?;
    if incompatible {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "package version is required by installed dependencies",
        )));
    }
    conn.execute("INSERT INTO package(name,author,version,title,description,license,repository,installed_at,upgraded_at,has_lib,has_app)
        VALUES(?1,?2,?3,?4,?5,?6,?7,unixepoch(),NULL,?8,?9) ON CONFLICT(name) DO UPDATE SET author=excluded.author,version=excluded.version,title=excluded.title,
        description=excluded.description,license=excluded.license,repository=excluded.repository,upgraded_at=unixepoch(),has_lib=excluded.has_lib,has_app=excluded.has_app",
        rusqlite::params![name,author,api["version"].as_str(),api["title"].as_str(),api["description"].as_str(),api["license"].as_str(),api["repository"].as_str(),api["has_lib"].as_bool().unwrap(),api["has_app"].as_bool().unwrap()])?;
    conn.execute(
        "DELETE FROM package_dependency WHERE package_name=?1",
        [name],
    )?;
    for (dependency, version) in declared_dependencies(api) {
        let (dependency_author, dependency_name) = dependency.split_once('/').unwrap();
        let matches: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM package WHERE author=?1 AND name=?2 AND version=?3)",
            rusqlite::params![dependency_author, dependency_name, version],
            |r| r.get(0),
        )?;
        if !matches {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("dependency {dependency}@{version} is not installed"),
            )));
        }
        check_dependency_cycle(conn, id, &dependency)?;
        conn.execute(
            "INSERT INTO package_dependency VALUES(?1,?2,?3)",
            rusqlite::params![name, dependency_name, version],
        )?;
    }
    // PUT /std/acl/permissions has the same full-replacement semantics. Keep
    // package-driven replacement inside this existing install transaction.
    conn.execute("DELETE FROM acl.lib_capability WHERE lib_name=?1", [name])?;
    if api["has_lib"] == true {
        let prefix = if author == Some("std.dhttp.net") {
            "/std".to_owned()
        } else {
            format!("/{name}")
        };
        for (capability, title, endpoints) in capability_rows(package)? {
            conn.execute(
                "INSERT INTO acl.lib_capability VALUES(?1,?2,?3)",
                rusqlite::params![name, capability, title],
            )?;
            for (method, path) in endpoints {
                conn.execute(
                    "INSERT INTO acl.lib_capability_operation VALUES(?1,?2,?3,?4)",
                    rusqlite::params![name, capability, method, format!("{prefix}{path}")],
                )?;
            }
        }
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
    let name = id.rsplit('/').next().unwrap();
    let mut pending = vec![dependency.rsplit('/').next().unwrap().to_owned()];
    let mut seen = std::collections::HashSet::new();
    while let Some(current) = pending.pop() {
        if current == name {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("cyclic backend dependency involving {id}"),
            )));
        }
        if !seen.insert(current.clone()) {
            continue;
        }
        pending.extend(
            conn.prepare("SELECT dependency_name FROM package_dependency WHERE package_name=?1")?
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

// Older accepted source packages carried OpenAPI in a metadata volume. Append
// that verified document to the runtime component; modern components stay exact.
fn runtime_wasm(package: &webc::Container) -> Result<Vec<u8>> {
    let mut backend = package
        .get_atom("backend")
        .ok_or_else(expanded_error)?
        .to_vec();
    let mut depth = 0;
    let mut embedded = false;
    for payload in wasmparser::Parser::new(0).parse_all(&backend) {
        match payload.map_err(|_| expanded_error())? {
            wasmparser::Payload::ModuleSection { .. }
            | wasmparser::Payload::ComponentSection { .. } => depth += 1,
            wasmparser::Payload::End(_) if depth > 0 => depth -= 1,
            wasmparser::Payload::CustomSection(section)
                if depth == 0 && section.name() == "pishoo:openapi" =>
            {
                embedded = true;
                break;
            }
            _ => {}
        }
    }
    if embedded {
        return Ok(backend);
    }
    let document = super::package_file(package, "/metadata/openapi.json")?;
    let name = b"pishoo:openapi";
    let mut section = vec![name.len() as u8];
    section.extend_from_slice(name);
    section.extend_from_slice(&document);
    backend.push(0);
    let mut size = section.len();
    while size >= 0x80 {
        backend.push((size as u8 & 0x7f) | 0x80);
        size >>= 7;
    }
    backend.push(size as u8);
    backend.extend_from_slice(&section);
    Ok(backend)
}

fn write_expanded(dir: &std::fs::File, bytes: &[u8], id: &str, kind: &str) -> Result<()> {
    let package = read_package(bytes)?;
    if kind == "lib" {
        write_component(
            dir,
            &format!("{}.wasm", id.rsplit('/').next().unwrap()),
            &runtime_wasm(&package)?,
        )?;
    } else {
        let mapping = package
            .manifest()
            .filesystem()
            .map_err(|_| expanded_error())?
            .unwrap();
        let mapping = &mapping[0];
        let volume = package
            .get_volume(&mapping.volume_name)
            .ok_or_else(expanded_error)?;
        let root = mapping
            .host_path
            .as_deref()
            .unwrap_or("/")
            .trim_end_matches('/');
        let mut pending = vec![format!("{root}/")];
        while let Some(path) = pending.pop() {
            for (name, _, entry) in volume.read_dir(path.as_str()).ok_or_else(expanded_error)? {
                let path = format!("{path}{name}");
                if matches!(entry, webc::Metadata::Dir { .. }) {
                    pending.push(format!("{path}/"));
                    continue;
                }
                let relative = path.strip_prefix(root).unwrap().trim_start_matches('/');
                if matches!(
                    relative.split('/').next(),
                    Some(".transaction.json" | ".previous")
                ) {
                    return Err(Error::InvalidComponent(
                        "reserved recovery path in App".into(),
                    ));
                }
                let target = relative.to_owned();
                let mut parent = dir.try_clone()?;
                let mut parts = target.split('/').peekable();
                while let Some(part) = parts.next() {
                    if parts.peek().is_some() {
                        match rustix::fs::mkdirat(
                            &parent,
                            part,
                            rustix::fs::Mode::from_raw_mode(0o700),
                        ) {
                            Ok(()) => parent.sync_all()?,
                            Err(rustix::io::Errno::EXIST) => {}
                            Err(error) => return Err(layout_error(error)),
                        }
                        let child = child_dir(&parent, std::ffi::OsStr::new(part))?;
                        inherit_owner(&child, &parent)?;
                        parent = child;
                    } else {
                        write_component(
                            &parent,
                            part,
                            &volume
                                .read_file(path.as_str())
                                .ok_or_else(expanded_error)?
                                .0,
                        )?;
                        parent.sync_all()?;
                    }
                }
            }
        }
    }
    dir.sync_all()?;
    Ok(())
}

fn expanded_bytes(dir: &std::fs::File) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut proof = Vec::new();
    expanded_file(dir, ".webc-proof")?
        .take(80 * 1024 * 1024 + 1)
        .read_to_end(&mut proof)?;
    restore_package(dir, &proof)
}

fn restore_package(dir: &std::fs::File, proof: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read;
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

pub(crate) fn disk_bytes(
    profile: &IdentityProfile,
    root: &std::fs::File,
    id: &str,
) -> Result<Vec<u8>> {
    // Database metadata and runtime payload publish in one management transaction.
    // Reopen the directory so this short read lock does not change a caller's
    // existing flock or remain held after returning the reconstructed bytes.
    let locked = child_dir(root, std::ffi::OsStr::new("."))?;
    rustix::fs::flock(&locked, rustix::fs::FlockOperation::LockShared)
        .map_err(std::io::Error::from)?;
    installed_bytes(profile, &locked, id, "lib")
}

fn installed_bytes(
    profile: &IdentityProfile,
    root: &std::fs::File,
    id: &str,
    kind: &str,
) -> Result<Vec<u8>> {
    let mut database = installation_database(profile, false)?;
    let conn = database.transaction()?;
    let table = match kind {
        "app" => "app",
        "lib" => "lib",
        _ => "package",
    };
    let modern: bool = conn.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM pragma_table_info('{table}') WHERE name='author')"),
        [],
        |row| row.get(0),
    )?;
    if !modern {
        return artifact_bytes(root, id, kind);
    }
    let runtime = if kind == "lib" {
        resource_parent(root, id, false).and_then(|parent| {
            expanded_file(&parent, &format!("{}.wasm", id.rsplit('/').next().unwrap()))
        })
    } else {
        let app = child_dir(root, std::ffi::OsStr::new(id.rsplit('/').next().unwrap()));
        match app {
            Ok(app) => {
                // Old ordinary Apps already used apps/<name>/public. Their
                // actual archive marker, not metadata columns, distinguishes
                // that preserved layout after another package migrates SQLite.
                for marker in [".webc-proof", "app.webc"] {
                    match expanded_file(&app, marker) {
                        Ok(_) => return artifact_bytes(root, id, kind),
                        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error),
                    }
                }
                Ok(app)
            }
            Err(error) => Err(error),
        }
    };
    match runtime {
        Ok(_) => runtime_snapshot(&conn, root, id, kind),
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            artifact_bytes(root, id, kind)
        }
        Err(error) => Err(error),
    }
}

fn snapshot_directory(
    dir: &std::fs::File,
    depth: usize,
    entries: &mut usize,
    size: &mut usize,
) -> Result<webc::v3::write::Directory<'static>> {
    use std::io::Read;

    use webc::v3::{
        Timestamps,
        write::{DirEntry, Directory, FileEntry},
    };
    if depth > 64 {
        return Err(expanded_error());
    }
    let mut children = std::collections::BTreeMap::new();
    let directory = cap_std::fs::Dir::from_std_file(dir.try_clone()?);
    for entry in directory.entries()? {
        let entry = entry?;
        *entries += 1;
        if *entries > 10000 {
            return Err(expanded_error());
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| expanded_error())?;
        if name == ".transaction.json" || name == ".previous" {
            continue;
        }
        let kind = entry.file_type()?;
        let value = if kind.is_dir() {
            let child = child_dir(dir, std::ffi::OsStr::new(&name))?;
            if child.metadata()?.dev() != dir.metadata()?.dev() {
                return Err(expanded_error());
            }
            DirEntry::Dir(snapshot_directory(&child, depth + 1, entries, size)?)
        } else if kind.is_file() {
            let file = expanded_file(dir, &name)?;
            if file.metadata()?.dev() != dir.metadata()?.dev() {
                return Err(expanded_error());
            }
            let mut bytes = Vec::new();
            file.take((64 * 1024 * 1024 - *size + 1) as u64)
                .read_to_end(&mut bytes)?;
            *size += bytes.len();
            if *size > 64 * 1024 * 1024 {
                return Err(expanded_error());
            }
            DirEntry::File(FileEntry::owned(bytes, Timestamps::default()))
        } else {
            return Err(expanded_error());
        };
        children.insert(name.parse().map_err(|_| expanded_error())?, value);
    }
    Ok(Directory::new(children, Timestamps::default()))
}

fn runtime_snapshot(
    conn: &rusqlite::Connection,
    root: &std::fs::File,
    id: &str,
    kind: &str,
) -> Result<Vec<u8>> {
    use std::io::Read;

    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use sha2::{Digest, Sha256};
    use webc::v3::{
        SignatureAlgorithm, Timestamps,
        write::{Directory, FileEntry, Writer},
    };
    let metadata = registered_metadata(conn, kind, id)?;
    let description = metadata["description"].as_str().ok_or_else(|| {
        Error::InvalidConfig("installed package is missing its description".into())
    })?;
    let license = metadata["license"]
        .as_str()
        .ok_or_else(|| Error::InvalidConfig("installed package is missing its license".into()))?;
    let mut wapm = serde_json::json!({"name":id,"version":metadata["version"],"description":description,"license":license});
    if let Some(repository) = metadata.get("repository").filter(|v| !v.is_null()) {
        wapm["repository"] = repository.clone();
    }
    let mut annotation =
        serde_json::json!({"title":metadata["title"],"dependencies":metadata["dependencies"]});
    if kind == "app" {
        annotation["entry"] = "/".into();
    }
    let mount = if kind == "app" { "public" } else { "metadata" };
    let mut backend = Vec::new();
    if kind == "lib" {
        expanded_file(
            &resource_parent(root, id, false)?,
            &format!("{}.wasm", id.rsplit('/').next().unwrap()),
        )?
        .take(64 * 1024 * 1024 + 1)
        .read_to_end(&mut backend)?;
        if backend.len() > 64 * 1024 * 1024 {
            return Err(expanded_error());
        }
    }
    let mut manifest = serde_json::json!({"package":{"wapm":wapm,"pishoo":annotation,
        "fs":[{"volume_name":mount,"mount_path":format!("/{mount}")}]},"atoms":{}});
    if kind == "lib" {
        manifest["atoms"]["backend"] = serde_json::json!({"kind":"https://webc.org/kind/wasm","signature":format!("sha256:{}",STANDARD.encode(Sha256::digest(&backend)))});
    }
    let manifest: webc::metadata::Manifest =
        serde_json::from_value(manifest).map_err(|_| expanded_error())?;
    let atoms = if kind == "lib" {
        std::collections::BTreeMap::from([(
            "backend".parse().unwrap(),
            FileEntry::owned(backend, Timestamps::default()),
        )])
    } else {
        std::collections::BTreeMap::new()
    };
    let mut writer = Writer::default()
        .write_manifest(&manifest)
        .map_err(|_| expanded_error())?
        .write_atoms(atoms)
        .map_err(|_| expanded_error())?;
    let volume = if kind == "app" {
        let app = child_dir(root, std::ffi::OsStr::new(id.rsplit('/').next().unwrap()))?;
        let public = app;
        snapshot_directory(&public, 0, &mut 0, &mut 0)?
    } else {
        Directory::default()
    };
    writer
        .write_volume(mount, volume)
        .map_err(|_| expanded_error())?;
    // This is an unsigned, installation-trusted runtime holder. It cannot recover
    // or validate the original signed source archive and never enters read_package.
    Ok(writer
        .finish(SignatureAlgorithm::None)
        .map_err(|_| expanded_error())?
        .to_vec())
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
    let resource = if kind == "app" {
        child_dir(root, std::ffi::OsStr::new(id.rsplit('/').next().unwrap()))
    } else {
        resource_dir(root, id)
    };
    let dir = match resource {
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

fn registered_metadata(
    conn: &rusqlite::Connection,
    _kind: &str,
    id: &str,
) -> Result<serde_json::Value> {
    use rusqlite::OptionalExtension;
    let mut value = conn.query_row("SELECT name,author,version,title,description,license,repository,installed_at,upgraded_at,has_lib,has_app FROM package WHERE (CASE WHEN author IS NULL THEN name ELSE author||'/'||name END)=?1", [id], |r| {
        let name: String = r.get(0)?;
        Ok(serde_json::json!({"id":id,"name":name,"author":r.get::<_,Option<String>>(1)?,"version":r.get::<_,String>(2)?,
            "title":r.get::<_,Option<String>>(3)?.unwrap_or(name),"description":r.get::<_,Option<String>>(4)?,"license":r.get::<_,Option<String>>(5)?,"repository":r.get::<_,Option<String>>(6)?,
            "installed_at":r.get::<_,Option<i64>>(7)?,"upgraded_at":r.get::<_,Option<i64>>(8)?,"has_lib":r.get::<_,bool>(9)?,"has_app":r.get::<_,bool>(10)?,"endpoints":[]}))
    }).optional()?.ok_or(Error::RouteNotFound)?;
    let dependencies = conn.prepare("SELECT p.author,p.name,d.version FROM package_dependency d JOIN package p ON p.name=d.dependency_name WHERE d.package_name=?1 ORDER BY p.author,p.name")?
        .query_map([id.rsplit('/').next().unwrap()], |r| Ok(serde_json::json!({"publisher":r.get::<_,Option<String>>(0)?,"name":r.get::<_,String>(1)?,"version":r.get::<_,String>(2)?})))?
        .collect::<std::result::Result<Vec<_>,_>>()?;
    value["dependencies"] = dependencies.into();
    if value["has_app"] == true {
        value["entry"] = "/".into();
    }
    Ok(value)
}

pub(crate) fn installed_packages(
    profile: &IdentityProfile,
    id: Option<&str>,
) -> Result<serde_json::Value> {
    let mut database = installation_database(profile, false)?;
    let tx = database.transaction()?;
    let rows = registered_packages(&tx, "pkg", id)?;
    let mut values = rows
        .into_iter()
        .map(|(id, _)| registered_metadata(&tx, "pkg", &id))
        .collect::<Result<Vec<_>>>()?;
    if id.is_some() {
        values.pop().ok_or(Error::RouteNotFound)
    } else {
        Ok(values.into())
    }
}

#[cfg(test)]
pub(crate) fn installed_libs(
    profile: &IdentityProfile,
    id: Option<&str>,
) -> Result<serde_json::Value> {
    if let Some(id) = id {
        valid_id(id)?;
    }
    let mut database = installation_database(profile, false)?;
    let conn = database.transaction()?;
    let mut values = Vec::new();
    for (registered, _) in registered_packages(&conn, "lib", id)? {
        values.push(registered_metadata(&conn, "lib", &registered)?);
    }
    if id.is_some() {
        Ok(values.pop().unwrap())
    } else {
        Ok(values.into())
    }
}

fn initialized_profile(profile: &IdentityProfile) -> Result<std::fs::File> {
    let dir = profile_dir(profile)?;
    crate::setup::open_config(profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    Ok(dir)
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
    inherit_owner(&file, dir)?;
    file.sync_all()?;
    drop(scopeguard::ScopeGuard::into_inner(file));
    Ok(())
}

pub(crate) fn install_package(
    profile: &IdentityProfile,
    id: &str,
    bytes: &[u8],
    runtime: &WasmRuntime,
) -> Result<serde_json::Value> {
    use rustix::fs::{FlockOperation, Mode, flock, mkdirat};
    valid_id(id)?;
    if id.rsplit('/').next() == Some("std") {
        return Err(Error::BadRequest("std is reserved".into()));
    }
    let parent = initialized_profile(profile)?;
    flock(&parent, FlockOperation::LockExclusive).map_err(std::io::Error::from)?;
    let package = read_package(bytes)?;
    let metadata = package_metadata(&package)?;
    check_resource_identity(&metadata, id)?;
    if let Some(backend) = package.get_atom("backend") {
        runtime.compile(&backend)?;
    }
    let mut database = installation_database(profile, true)?;
    check_name(&database, id, "pkg")?;
    let same_version = registered_packages(&database, "pkg", Some(id))
        .ok()
        .and_then(|r| r.first().map(|r| r.1.clone()))
        .as_deref()
        == metadata["version"].as_str();
    let name = temporary_name()?;
    mkdirat(&parent, &name, Mode::from_raw_mode(0o700)).map_err(layout_error)?;
    let cleanup_dir = cap_std::fs::Dir::from_std_file(parent.try_clone()?);
    let cleanup = scopeguard::guard((), |_| {
        let _ = cleanup_dir.remove_dir_all(&name);
    });
    let stage = child_dir(&parent, std::ffi::OsStr::new(&name))?;
    inherit_owner(&stage, &parent)?;
    for kind in ["lib", "app"] {
        if metadata[format!("has_{kind}")] != true {
            continue;
        }
        mkdirat(&stage, format!("new-{kind}"), Mode::from_raw_mode(0o700)).map_err(layout_error)?;
        let dir = child_dir(&stage, std::ffi::OsStr::new(&format!("new-{kind}")))?;
        inherit_owner(&dir, &parent)?;
        write_expanded(&dir, bytes, id, kind)?;
    }
    let tx = database.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    for kind in ["lib", "app"] {
        if metadata[format!("has_{kind}")] == true {
            check_routes(profile, &tx, id, kind, &package)?;
            check_existing_rules(profile, &tx, id, kind, &package, same_version)?;
        }
    }
    save_registration(&tx, id, &metadata, &package)?;
    scopeguard::ScopeGuard::into_inner(cleanup);
    commit_package_files(profile, id, &parent, &stage, &name, tx)?;
    Ok(lib_metadata(&package, Some(id)))
}

// Both runtime parts share one SQLite decision and one durable recovery journal.
fn commit_package_files(
    profile: &IdentityProfile,
    id: &str,
    parent: &std::fs::File,
    stage: &std::fs::File,
    name: &str,
    tx: rusqlite::Transaction<'_>,
) -> Result<()> {
    use rustix::fs::{Mode, mkdirat, renameat};
    let current = profile_dir(profile)?;
    if current.metadata()?.dev() != parent.metadata()?.dev()
        || current.metadata()?.ino() != parent.metadata()?.ino()
    {
        return Err(Error::InvalidConfig(
            "identity directory changed during package installation".into(),
        ));
    }
    let short = id.rsplit('/').next().unwrap();
    let mut targets = Vec::new();
    let mut resources = Vec::new();
    for kind in ["lib", "app"] {
        match mkdirat(parent, kind, Mode::from_raw_mode(0o700)) {
            Ok(()) => {}
            Err(rustix::io::Errno::EXIST) => {}
            Err(e) => return Err(layout_error(e)),
        }
        let root = child_dir(parent, std::ffi::OsStr::new(kind))?;
        inherit_owner(&root, parent)?;
        let target = if kind == "lib" {
            resource_parent(&root, id, true)?
        } else {
            root
        };
        if target.metadata()?.dev() != parent.metadata()?.dev() {
            return Err(expanded_error());
        }
        let leaf = if kind == "lib" {
            format!("{short}.wasm")
        } else {
            short.to_owned()
        };
        let old = match rustix::fs::statat(
            &target,
            leaf.as_str(),
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        ) {
            Ok(stat) => {
                if kind == "lib" {
                    if !expanded_file(&target, &leaf)?.metadata()?.is_file() || stat.st_nlink != 1 {
                        return Err(expanded_error());
                    }
                } else {
                    child_dir(&target, std::ffi::OsStr::new(&leaf))?;
                }
                true
            }
            Err(rustix::io::Errno::NOENT) => false,
            Err(e) => return Err(layout_error(e)),
        };
        let new = match child_dir(stage, std::ffi::OsStr::new(&format!("new-{kind}"))) {
            Ok(_) => true,
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error),
        };
        resources.push(serde_json::json!({"kind":kind,"old":old,"new":new}));
        targets.push((target, leaf, old, new));
    }
    let facts: String = tx.query_row(include_str!("registration-facts.sql"), [short], |r| {
        r.get(0)
    })?;
    write_component(
        stage,
        ".transaction.json",
        &serde_json::to_vec(&serde_json::json!({"id":id,"resources":resources,"facts":facts}))
            .map_err(|e| Error::InvalidConfig(e.to_string()))?,
    )?;
    stage.sync_all()?;
    parent.sync_all()?;
    let result = (|| -> Result<()> {
        for ((target, leaf, old, new), kind) in targets.iter().zip(["lib", "app"]) {
            if *old {
                renameat(target, leaf.as_str(), stage, format!("old-{kind}"))
                    .map_err(layout_error)?;
            }
            if *new {
                let dir = child_dir(stage, std::ffi::OsStr::new(&format!("new-{kind}")))?;
                if kind == "lib" {
                    renameat(&dir, leaf.as_str(), target, leaf.as_str()).map_err(layout_error)?;
                } else {
                    renameat(stage, "new-app", target, leaf.as_str()).map_err(layout_error)?;
                }
            }
            target.sync_all()?;
            stage.sync_all()?;
        }
        database_owner(profile)?;
        tx.commit()?;
        Ok(())
    })();
    if let Err(error) = result {
        let rollback = (|| -> Result<()> {
            for ((target, leaf, old, new), kind) in targets.iter().zip(["lib", "app"]) {
                let backup = format!("old-{kind}");
                let has_backup = rustix::fs::statat(
                    stage,
                    backup.as_str(),
                    rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
                )
                .is_ok();
                let source_exists = if kind == "lib" {
                    child_dir(stage, std::ffi::OsStr::new("new-lib"))
                        .and_then(|d| expanded_file(&d, leaf))
                        .is_ok()
                } else {
                    child_dir(stage, std::ffi::OsStr::new("new-app")).is_ok()
                };
                if has_backup || (!*old && *new && !source_exists) {
                    let dir = cap_std::fs::Dir::from_std_file(target.try_clone()?);
                    if kind == "lib" {
                        let _ = dir.remove_file(leaf);
                    } else {
                        let _ = dir.remove_dir_all(leaf);
                    }
                }
                if has_backup {
                    renameat(stage, backup.as_str(), target, leaf.as_str())
                        .map_err(layout_error)?;
                }
                target.sync_all()?;
            }
            Ok(())
        })();
        if rollback.is_ok() {
            let _ = cap_std::fs::Dir::from_std_file(parent.try_clone()?).remove_dir_all(name);
        }
        return Err(error);
    }
    if let Err(error) = cap_std::fs::Dir::from_std_file(parent.try_clone()?).remove_dir_all(name) {
        tracing::warn!(%error,"committed package cleanup deferred to startup");
    }
    if let Err(error) = parent.sync_all() {
        tracing::warn!(%error,"package cleanup sync failed");
    }
    Ok(())
}

pub(crate) fn remove_package(profile: &IdentityProfile, id: &str) -> Result<()> {
    use rustix::fs::{FlockOperation, Mode, flock, mkdirat};
    valid_id(id)?;
    let parent = initialized_profile(profile)?;
    flock(&parent, FlockOperation::LockExclusive).map_err(std::io::Error::from)?;
    let mut database = installation_database(profile, true)?;
    if matches!(
        registered_packages(&database, "pkg", Some(id)),
        Err(Error::RouteNotFound)
    ) {
        return Ok(());
    }
    let short = id.rsplit('/').next().unwrap();
    let name = temporary_name()?;
    mkdirat(&parent, &name, Mode::from_raw_mode(0o700)).map_err(layout_error)?;
    let stage = child_dir(&parent, std::ffi::OsStr::new(&name))?;
    let tx = database.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let referenced: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM package_dependency WHERE dependency_name=?1)",
        [short],
        |r| r.get(0),
    )?;
    if referenced {
        cap_std::fs::Dir::from_std_file(parent).remove_dir_all(&name)?;
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "package is required by installed dependencies",
        )));
    }
    // DELETE /std/acl/permissions semantics, committed atomically with removal.
    // Existing access_rules and the Lib's private business data are retained.
    tx.execute("DELETE FROM acl.lib_capability WHERE lib_name=?1", [short])?;
    tx.execute("DELETE FROM package WHERE name=?1", [short])?;
    commit_package_files(profile, id, &parent, &stage, &name, tx)
}

#[cfg(test)]
pub(crate) fn install_lib(
    profile: &IdentityProfile,
    id: &str,
    bytes: &[u8],
    runtime: &WasmRuntime,
) -> Result<serde_json::Value> {
    install_package(profile, id, bytes, runtime)
}
#[cfg(test)]
pub(crate) fn install_app(
    profile: &IdentityProfile,
    id: &str,
    bytes: &[u8],
) -> Result<serde_json::Value> {
    install_package(profile, id, bytes, &WasmRuntime::new()?)
}
#[cfg(test)]
pub(crate) fn remove_lib(profile: &IdentityProfile, id: &str) -> Result<()> {
    remove_package(profile, id)
}
#[cfg(test)]
pub(crate) fn remove_app(profile: &IdentityProfile, id: &str) -> Result<()> {
    remove_package(profile, id)
}

/// Read installed frontend resources without repeating installation validation.
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
    for (id, _) in ids {
        let package = webc::Container::from_bytes_and_version(
            installed_bytes(profile, &root, &id, "app")?.into(),
            webc::Version::V3,
        )
        .map_err(|error| Error::InvalidComponent(error.to_string()))?;
        package_metadata(&package)?;
        apps.insert(id, package);
    }
    Ok(apps)
}
