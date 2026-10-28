//! Package files: directory traversal, queries, atomic installation and removal.
//! Opened directory handles refuse symlinks throughout traversal.

use std::os::unix::fs::MetadataExt;

use dhttp_home::identity::IdentityProfile;

#[cfg(test)]
use super::{WasmRuntime, lib_metadata};
use super::{package_metadata, read_package};
use crate::{Error, Result};

#[cfg(test)]
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
        tx.execute_batch(include_str!("sql/schema-v3.sql"))?;
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
#[allow(
    clippy::type_complexity,
    reason = "Keep the frozen capability tuple interface unchanged."
)]
fn capability_rows(
    package: &super::PackageFiles,
) -> Result<Vec<(String, String, Vec<(String, String)>)>> {
    let document: serde_json::Value = serde_json::from_slice(super::package_openapi(package)?)
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
                group["title"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| group["title"].to_string()),
                endpoints,
            ));
        }
    }
    rows.sort();
    Ok(rows)
}

fn package_routes(
    package: &super::PackageFiles,
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
        let api = super::validate_openapi(super::package_openapi(package)?)?;
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
        for path in package
            .keys()
            .filter_map(|path| path.strip_prefix("public/"))
        {
            let suffix = if path == "index.html" {
                String::new()
            } else {
                path.to_owned()
            };
            routes.insert(
                format!("/app/{}/{}", id.rsplit('/').next().unwrap(), suffix),
                vec!["GET".into(), "HEAD".into()],
            );
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
    package: &super::PackageFiles,
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
        for (other, _) in registered_packages(conn, other_kind, None)? {
            if other == id {
                continue;
            }
            let root = child_dir(&profile_dir(profile)?, std::ffi::OsStr::new(other_kind))?;
            for (path, methods) in installed_routes(&root, &other, other_kind)? {
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

fn installed_routes(
    root: &std::fs::File,
    id: &str,
    kind: &str,
) -> Result<std::collections::BTreeMap<String, Vec<String>>> {
    if kind == "lib" {
        let file = expanded_file(
            &resource_parent(root, id, false)?,
            &format!("{}.wasm", id.rsplit('/').next().unwrap()),
        )?;
        return package_routes(
            &std::collections::BTreeMap::from([("backend.wasm".into(), read_runtime_wasm(file)?)]),
            id,
            kind,
        );
    }
    let directory = child_dir(root, std::ffi::OsStr::new(id.rsplit('/').next().unwrap()))?;
    let device = directory.metadata()?.dev();
    let mut files = std::collections::BTreeMap::new();
    let mut pending = vec![(directory, String::new(), 0)];
    let mut count = 0;
    while let Some((directory, prefix, depth)) = pending.pop() {
        if depth > 32 {
            return Err(expanded_error());
        }
        for entry in cap_std::fs::Dir::from_std_file(directory.try_clone()?).entries()? {
            let entry = entry?;
            count += 1;
            if count > 10000 {
                return Err(expanded_error());
            }
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| expanded_error())?;
            if matches!(name.as_str(), ".transaction.json" | ".previous") {
                continue;
            }
            let path = format!("{prefix}{name}");
            if entry.file_type()?.is_dir() {
                let child = child_dir(&directory, std::ffi::OsStr::new(&name))?;
                if child.metadata()?.dev() != device {
                    return Err(expanded_error());
                }
                pending.push((child, format!("{path}/"), depth + 1));
            } else if entry.file_type()?.is_file() {
                let file = expanded_file(&directory, &name)?;
                if file.metadata()?.dev() != device {
                    return Err(expanded_error());
                }
                // Only the resource names are needed for route collision checks.
                files.insert(format!("public/{path}"), Vec::new());
            } else {
                return Err(expanded_error());
            }
        }
    }
    package_routes(&files, id, kind)
}

// An existing package keeps its path-based ACLs as APIs evolve. Only a fresh
// provider must review rules left behind before taking over those operations.
fn check_existing_rules(
    profile: &IdentityProfile,
    conn: &rusqlite::Connection,
    id: &str,
    kind: &str,
    package: &super::PackageFiles,
    same_version: bool,
) -> Result<()> {
    if same_version {
        return Ok(());
    }

    // Use the unified package identity, including upgrades that add an App or
    // Lib to the same author's already installed package.
    match registered_packages(conn, "pkg", Some(id)) {
        Ok(_) => return Ok(()),
        Err(Error::RouteNotFound) => {}
        Err(e) => return Err(e),
    }
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
            // App reads use root GET authorization, including HEAD and assets.
            let method = if kind == "app" && matches!(method.as_str(), "GET" | "HEAD") {
                "GET"
            } else {
                method.as_str()
            };
            let path = if kind == "app" && method == "GET" {
                format!("/app/{}", id.rsplit('/').next().unwrap())
            } else {
                path.trim_end_matches('/').to_owned()
            };
            let exists: bool = access.query_row(
                "SELECT EXISTS(SELECT 1 FROM access_rules WHERE (api='/' OR api=?1 OR substr(?1,1,length(api)+1)=api||'/') AND method IN (?2,'*'))",
                rusqlite::params![path, method],
                |row| row.get(0),
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
    conn: &rusqlite::Transaction<'_>,
    id: &str,
    api: &serde_json::Value,
    package: &super::PackageFiles,
) -> Result<()> {
    check_name(conn, id, "pkg")?;
    let name = id.rsplit('/').next().unwrap();
    let author = id.split_once('/').map(|(author, _)| author);
    let required = conn.prepare(
        "SELECT CASE WHEN p.author IS NULL THEN p.name ELSE p.author||'/'||p.name END,d.version FROM package_dependency d JOIN package p ON p.name=d.package_name WHERE d.dependency_name=?1 AND d.version<>?2 ORDER BY p.name",
    )?.query_map(rusqlite::params![name, api["version"].as_str().unwrap()], |r| {
        Ok(format!("{} requires {id}@{}", r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?.collect::<rusqlite::Result<Vec<_>>>()?;
    if !required.is_empty() {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "package version is required by installed dependencies: {}",
                required.join("; ")
            ),
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
    let mut groups = if api["has_lib"] == true {
        capability_rows(package)?
    } else {
        Vec::new()
    };
    if !groups.is_empty() {
        let prefix = if author == Some("std.dhttp.net") {
            "/std".to_owned()
        } else {
            format!("/{name}")
        };
        for (_, _, endpoints) in &mut groups {
            for (_, path) in endpoints {
                *path = format!("{prefix}{path}");
            }
        }
    }
    // App authority comes from this validated package, never from a guest path.
    if api["has_lib"] == true {
        let document: serde_json::Value = serde_json::from_slice(super::package_openapi(package)?)
            .map_err(|e| Error::InvalidComponent(e.to_string()))?;
        let declarations = document
            .get("x-access-groups")
            .or_else(|| document["x-pishoo"].get("access-groups"));
        for (id, _, endpoints) in &mut groups {
            if declarations
                .and_then(|groups| groups.get(id.as_str()))
                .is_some_and(|group| group["app"] == true)
            {
                if api["has_app"] != true {
                    return Err(Error::InvalidComponent(
                        "app capability requires this package's public resources".into(),
                    ));
                }
                endpoints.push(("GET".into(), format!("/app/{name}")));
            }
        }
    }
    crate::routes::permissions(conn, name, &groups)
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

fn write_expanded(dir: &std::fs::File, bytes: &[u8], id: &str, kind: &str) -> Result<()> {
    let package = read_package(bytes)?;
    if kind == "lib" {
        write_component(
            dir,
            &format!("{}.wasm", id.rsplit('/').next().unwrap()),
            package.get("backend.wasm").ok_or_else(expanded_error)?,
        )?;
    } else {
        for (path, data) in &package {
            let Some(relative) = path.strip_prefix("public/") else {
                continue;
            };
            if matches!(
                relative.split('/').next(),
                Some(".transaction.json" | ".previous")
            ) {
                return Err(Error::InvalidComponent(
                    "reserved recovery path in App".into(),
                ));
            }
            let mut parent = dir.try_clone()?;
            let mut parts = relative.split('/').peekable();
            while let Some(part) = parts.next() {
                if parts.peek().is_some() {
                    match rustix::fs::mkdirat(&parent, part, rustix::fs::Mode::from_raw_mode(0o700))
                    {
                        Ok(()) => parent.sync_all()?,
                        Err(rustix::io::Errno::EXIST) => {}
                        Err(error) => return Err(layout_error(error)),
                    }
                    let child = child_dir(&parent, std::ffi::OsStr::new(part))?;
                    inherit_owner(&child, &parent)?;
                    parent = child;
                } else {
                    write_component(&parent, part, data)?;
                    parent.sync_all()?;
                }
            }
        }
    }
    dir.sync_all()?;
    Ok(())
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
    valid_id(id)?;
    let mut database = installation_database(profile, false)?;
    let conn = database.transaction()?;
    if registered_metadata(&conn, "lib", id)?["has_lib"] != true {
        return Err(Error::RouteNotFound);
    }
    let file = resource_parent(&locked, id, false).and_then(|parent| {
        expanded_file(&parent, &format!("{}.wasm", id.rsplit('/').next().unwrap()))
    });
    read_runtime_wasm(file?)
}

fn read_runtime_wasm(file: std::fs::File) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    file.take(64 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 64 * 1024 * 1024 {
        return Err(Error::InvalidComponent("component exceeds 64 MiB".into()));
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

/// Install a verified dependency closure in dependency order, with its requested
/// root last. The caller holds the existing Sandbox write lock throughout.
pub(super) fn install_packages(
    profile: &IdentityProfile,
    packages: &[(String, Vec<u8>)],
    sandbox: &mut super::Sandbox,
) -> Result<()> {
    use std::sync::Arc;

    use rustix::fs::{FlockOperation, Mode, flock, mkdirat};

    if packages.is_empty() || packages.len() > 100 {
        return Err(Error::BadRequest(
            "installation requires 1 to 100 packages".into(),
        ));
    }
    if sandbox.tasks.is_closed() {
        return Err(Error::Denied);
    }
    let parent = initialized_profile(profile)?;
    flock(&parent, FlockOperation::LockExclusive).map_err(std::io::Error::from)?;
    let mut database = installation_database(profile, true)?;
    let tx = database.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut names = std::collections::HashSet::new();
    let mut changed = Vec::new();
    for (index, (id, bytes)) in packages.iter().enumerate() {
        valid_id(id)?;
        let short = id.rsplit('/').next().unwrap();
        if short == "std" || !names.insert(short) {
            return Err(Error::BadRequest(
                "reserved or duplicate package name".into(),
            ));
        }
        let package = read_package(bytes)?;
        let metadata = package_metadata(&package)?;
        check_resource_identity(&metadata, id)?;
        check_name(&tx, id, "pkg")?;
        let current = match registered_metadata(&tx, "pkg", id) {
            Ok(current) => Some(current),
            Err(Error::RouteNotFound) => None,
            Err(error) => return Err(error),
        };
        let same_version = current
            .as_ref()
            .is_some_and(|v| v["version"] == metadata["version"]);
        let reused = index + 1 != packages.len()
            && same_version
            && current.as_ref().is_some_and(|v| {
                v["has_lib"] == metadata["has_lib"] && v["has_app"] == metadata["has_app"]
            })
            && (metadata["has_lib"] != true
                || sandbox.libs.get(id).is_some_and(|lib| {
                    Some(lib.openapi.info.version.as_str()) == metadata["version"].as_str()
                }))
            && (metadata["has_app"] != true
                || sandbox.apps.get(id).is_some_and(|(version, _)| {
                    Some(version.as_str()) == metadata["version"].as_str()
                }));
        if reused {
            continue;
        }
        // All route/ACL checks see the old, consistent installation. No files
        // or registrations have been switched while examining another package.
        for kind in ["lib", "app"] {
            if metadata[format!("has_{kind}")] == true {
                check_routes(profile, &tx, id, kind, &package)?;
                check_existing_rules(profile, &tx, id, kind, &package, same_version)?;
            }
        }
        changed.push((id, bytes, package, metadata));
    }
    // Only this operation's outgoing constraints are replaced. Dependents
    // outside the update retain their exact requirements and can block it.
    for (id, _, _, _) in &changed {
        tx.execute(
            "DELETE FROM package_dependency WHERE package_name=?1",
            [id.rsplit('/').next().unwrap()],
        )?;
    }
    for (id, _, package, metadata) in &changed {
        save_registration(&tx, id, metadata, package)?;
    }

    let name = temporary_name()?;
    mkdirat(&parent, &name, Mode::from_raw_mode(0o700)).map_err(layout_error)?;
    let cleanup_dir = cap_std::fs::Dir::from_std_file(parent.try_clone()?);
    let cleanup = scopeguard::guard((), |_| {
        let _ = cleanup_dir.remove_dir_all(&name);
    });
    let stage = child_dir(&parent, std::ffi::OsStr::new(&name))?;
    inherit_owner(&stage, &parent)?;
    let mut stages = Vec::new();
    let mut libs = std::collections::HashMap::new();
    let mut apps = std::collections::HashMap::new();
    for (id, bytes, _, metadata) in &changed {
        let short = id.rsplit('/').next().unwrap();
        mkdirat(&stage, short, Mode::from_raw_mode(0o700)).map_err(layout_error)?;
        let package_stage = child_dir(&stage, std::ffi::OsStr::new(short))?;
        inherit_owner(&package_stage, &parent)?;
        for kind in ["lib", "app"] {
            if metadata[format!("has_{kind}")] != true {
                continue;
            }
            let candidate = format!("new-{kind}");
            mkdirat(
                &package_stage,
                candidate.as_str(),
                Mode::from_raw_mode(0o700),
            )
            .map_err(layout_error)?;
            let dir = child_dir(&package_stage, std::ffi::OsStr::new(&candidate))?;
            inherit_owner(&dir, &parent)?;
            write_expanded(&dir, bytes, id, kind)?;
            let payload = if kind == "lib" {
                expanded_file(&dir, &format!("{short}.wasm"))?
            } else {
                dir
            };
            if kind == "lib" {
                let wasm = read_runtime_wasm(payload)?;
                let data = profile
                    .join(".volume")
                    .join(if id.starts_with("std.dhttp.net/") {
                        format!("std/{short}")
                    } else {
                        short.to_owned()
                    });
                libs.insert(
                    (*id).clone(),
                    Arc::new(crate::sandbox::Lib::load(
                        sandbox.runtime.clone(),
                        (*id).clone(),
                        &wasm,
                        &data,
                    )?),
                );
            } else {
                apps.insert(
                    (*id).clone(),
                    (
                        metadata["version"]
                            .as_str()
                            .ok_or_else(expanded_error)?
                            .to_owned(),
                        std::sync::Arc::new(payload),
                    ),
                );
            }
        }
        package_stage.sync_all()?;
        stages.push(package_stage);
    }
    let entries = changed
        .iter()
        .zip(&stages)
        .map(|((id, _, _, _), dir)| (id.as_str(), id.rsplit('/').next().unwrap(), dir))
        .collect::<Vec<_>>();
    scopeguard::ScopeGuard::into_inner(cleanup);
    commit_packages_files(profile, &entries, &parent, &stage, &name, tx)?;
    // Everything below is infallible. New requests see the whole new graph;
    // in-flight requests continue to own their original Arc<Lib> snapshots.
    for (id, _, _, _) in changed {
        sandbox.libs.remove(id);
        sandbox.apps.remove(id);
    }
    sandbox.libs.extend(libs);
    sandbox.apps.extend(apps);
    Ok(())
}

// Isolated acceptance still installs individual signed files directly. Online
// installation always uses the complete dependency transaction above.
#[cfg(test)]
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
    if let Some(backend) = package.get("backend.wasm") {
        runtime.compile(backend)?;
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

// Single-package installation and removal use the same commit/recovery path.
fn commit_package_files(
    profile: &IdentityProfile,
    id: &str,
    parent: &std::fs::File,
    stage: &std::fs::File,
    name: &str,
    tx: rusqlite::Transaction<'_>,
) -> Result<()> {
    commit_packages_files(profile, &[(id, "", stage)], parent, stage, name, tx)
}

// All packages and both runtime parts share one SQLite decision and one durable
// recovery journal. Entries refer to owned staging directories under `stage`.
fn commit_packages_files(
    profile: &IdentityProfile,
    entries: &[(&str, &str, &std::fs::File)],
    parent: &std::fs::File,
    stage: &std::fs::File,
    name: &str,
    tx: rusqlite::Transaction<'_>,
) -> Result<()> {
    use rustix::fs::{Mode, mkdirat, renameat};
    let cleanup_dir = cap_std::fs::Dir::from_std_file(parent.try_clone()?);
    let cleanup = scopeguard::guard((), |_| {
        let _ = cleanup_dir.remove_dir_all(name);
    });
    let current = profile_dir(profile)?;
    if current.metadata()?.dev() != parent.metadata()?.dev()
        || current.metadata()?.ino() != parent.metadata()?.ino()
    {
        return Err(Error::InvalidConfig(
            "identity directory changed during package installation".into(),
        ));
    }
    let mut targets = Vec::new();
    let mut packages = Vec::new();
    for &(id, directory, package_stage) in entries {
        let short = id.rsplit('/').next().unwrap();
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
                        if !expanded_file(&target, &leaf)?.metadata()?.is_file()
                            || stat.st_nlink != 1
                        {
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
            let new = match child_dir(package_stage, std::ffi::OsStr::new(&format!("new-{kind}"))) {
                Ok(_) => true,
                Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => return Err(error),
            };
            resources.push(serde_json::json!({"kind":kind,"old":old,"new":new}));
            targets.push((package_stage, kind, target, leaf, old, new));
        }
        let facts: String = tx.query_row(
            include_str!("sql/registration-facts-v3.sql"),
            [short],
            |r| r.get(0),
        )?;
        packages.push(
            serde_json::json!({"id":id,"directory":directory,"resources":resources,"facts":facts}),
        );
    }
    let journal = serde_json::to_vec(&serde_json::json!({"packages":packages}))
        .map_err(|e| Error::InvalidConfig(e.to_string()))?;
    if journal.len() > 2 * 1024 * 1024 {
        return Err(Error::BadRequest(
            "package recovery journal exceeds 2 MiB".into(),
        ));
    }
    write_component(stage, ".transaction.json", &journal)?;
    stage.sync_all()?;
    parent.sync_all()?;
    // After publishing the journal, a failed rollback must retain its backups
    // for startup recovery rather than deleting the only surviving old files.
    scopeguard::ScopeGuard::into_inner(cleanup);
    let cleanup = || -> Result<()> {
        // Invalidate the journal before removing any member directory. A crash
        // during cleanup must not leave a journal pointing at deleted backups.
        rustix::fs::unlinkat(stage, ".transaction.json", rustix::fs::AtFlags::empty())
            .map_err(layout_error)?;
        stage.sync_all()?;
        cleanup_dir.remove_dir_all(name)?;
        parent.sync_all()?;
        Ok(())
    };
    let result = (|| -> Result<()> {
        for &(package_stage, kind, ref target, ref leaf, old, new) in &targets {
            if old {
                renameat(target, leaf.as_str(), package_stage, format!("old-{kind}"))
                    .map_err(layout_error)?;
            }
            if new {
                let dir = child_dir(package_stage, std::ffi::OsStr::new(&format!("new-{kind}")))?;
                if kind == "lib" {
                    renameat(&dir, leaf.as_str(), target, leaf.as_str()).map_err(layout_error)?;
                    dir.sync_all()?;
                } else {
                    renameat(package_stage, "new-app", target, leaf.as_str())
                        .map_err(layout_error)?;
                }
            }
            target.sync_all()?;
            package_stage.sync_all()?;
        }
        database_owner(profile)?;
        tx.commit()?;
        Ok(())
    })();
    if let Err(error) = result {
        let rollback = (|| -> Result<()> {
            for &(package_stage, kind, ref target, ref leaf, old, new) in &targets {
                let backup = format!("old-{kind}");
                let has_backup = rustix::fs::statat(
                    package_stage,
                    backup.as_str(),
                    rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
                )
                .is_ok();
                let source_exists = if kind == "lib" {
                    child_dir(package_stage, std::ffi::OsStr::new("new-lib"))
                        .and_then(|d| expanded_file(&d, leaf))
                        .is_ok()
                } else {
                    child_dir(package_stage, std::ffi::OsStr::new("new-app")).is_ok()
                };
                if has_backup || (!old && new && !source_exists) {
                    let dir = cap_std::fs::Dir::from_std_file(target.try_clone()?);
                    let removed = if kind == "lib" {
                        dir.remove_file(leaf)
                    } else {
                        dir.remove_dir_all(leaf)
                    };
                    if let Err(error) = removed
                        && error.kind() != std::io::ErrorKind::NotFound
                    {
                        return Err(error.into());
                    }
                }
                if has_backup {
                    renameat(package_stage, backup.as_str(), target, leaf.as_str())
                        .map_err(layout_error)?;
                }
                target.sync_all()?;
                package_stage.sync_all()?;
            }
            Ok(())
        })();
        if rollback.is_ok()
            && let Err(error) = cleanup()
        {
            tracing::warn!(%error,"rolled back package cleanup deferred to startup");
        }
        return Err(error);
    }
    if let Err(error) = cleanup() {
        tracing::warn!(%error,"committed package cleanup deferred to startup");
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
    // Use the same permissions operation as HTTP DELETE, within this removal.
    // Existing access_rules and the Lib's private business data are retained.
    crate::routes::permissions(&tx, short, &[])?;
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

/// Open registered frontend directories without loading their contents.
pub(crate) fn installed_apps(
    profile: &IdentityProfile,
    id: Option<&str>,
) -> Result<std::collections::HashMap<String, (String, std::sync::Arc<std::fs::File>)>> {
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
        let metadata = registered_metadata(&conn, "app", &id)?;
        let version = metadata["version"]
            .as_str()
            .ok_or_else(expanded_error)?
            .to_owned();
        let directory = child_dir(&root, std::ffi::OsStr::new(id.rsplit('/').next().unwrap()))?;
        if directory.metadata()?.dev() != root.metadata()?.dev() {
            return Err(expanded_error());
        }
        apps.insert(id, (version, std::sync::Arc::new(directory)));
    }
    Ok(apps)
}

/// Open one static resource beneath the retained directory, never following links.
pub(crate) fn app_file(root: &std::fs::File, path: &str) -> Result<std::fs::File> {
    let device = root.metadata()?.dev();
    let mut directory = root.try_clone()?;
    let mut parts = path.split('/').peekable();
    while let Some(part) = parts.next() {
        if part.is_empty()
            || matches!(part, "." | ".." | ".transaction.json" | ".previous")
            || part.contains(['\\', '\0'])
        {
            return Err(Error::RouteNotFound);
        }
        if parts.peek().is_some() {
            directory = child_dir(&directory, std::ffi::OsStr::new(part))
                .map_err(|_| Error::RouteNotFound)?;
            if directory.metadata()?.dev() != device {
                return Err(Error::RouteNotFound);
            }
        } else {
            let file = expanded_file(&directory, part).map_err(|_| Error::RouteNotFound)?;
            if file.metadata()?.dev() != device {
                return Err(Error::RouteNotFound);
            }
            return Ok(file);
        }
    }
    Err(Error::RouteNotFound)
}
