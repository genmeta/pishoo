//! Package files: directory traversal, queries, atomic installation and removal.
//! Opened directory handles refuse symlinks throughout traversal.

use std::os::unix::fs::MetadataExt;

use dhttp_home::identity::IdentityProfile;
use rusqlite::OptionalExtension;

use super::{package_metadata, package_name};
use crate::{Error, Result};

#[cfg(test)]
#[path = "../../tests/unit/package/files/tests.rs"]
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

pub(crate) fn app_name(id: &str) -> &str {
    let name = package_name(id);
    name.strip_prefix("std/").unwrap_or(name)
}

/// Match the ordinary App mount and the old mount used by signed frontend assets.
pub(crate) fn app_mount<'a>(id: &'a str, tail: &str) -> Option<&'a str> {
    let name = app_name(id);
    let matches = |name: &str| {
        tail.strip_prefix(name)
            .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with('/'))
    };
    [name, package_name(id)]
        .into_iter()
        .find(|name| matches(name))
}

// Walk every path component through a directory handle before using a resource.
fn resource_directory(root: &std::fs::File, path: &str, create: bool) -> Result<std::fs::File> {
    let mut directory = root.try_clone()?;
    for part in path.split('/').filter(|part| !part.is_empty()) {
        if matches!(part, "." | "..") || part.contains(['\\', '\0']) {
            return Err(expanded_error());
        }
        if create {
            match rustix::fs::mkdirat(&directory, part, rustix::fs::Mode::from_raw_mode(0o700)) {
                Ok(()) => directory.sync_all()?,
                Err(rustix::io::Errno::EXIST) => {}
                Err(error) => return Err(layout_error(error)),
            }
        }
        let child = child_dir(&directory, std::ffi::OsStr::new(part))?;
        if child.metadata()?.dev() != root.metadata()?.dev() {
            return Err(expanded_error());
        }
        if create {
            inherit_owner(&child, &directory)?;
        }
        directory = child;
    }
    Ok(directory)
}

fn resource_location<'a>(
    root: &std::fs::File,
    path: &'a str,
    create: bool,
) -> Result<(std::fs::File, &'a str)> {
    let (parent, leaf) = path.rsplit_once('/').unwrap_or(("", path));
    if leaf.is_empty() || matches!(leaf, "." | "..") || leaf.contains(['\\', '\0']) {
        return Err(expanded_error());
    }
    Ok((resource_directory(root, parent, create)?, leaf))
}

fn resource_stat(root: &std::fs::File, path: &str) -> Result<rustix::fs::Stat> {
    let (parent, leaf) = resource_location(root, path, false)?;
    rustix::fs::statat(&parent, leaf, rustix::fs::AtFlags::SYMLINK_NOFOLLOW).map_err(layout_error)
}

fn rename_resource(
    source: &std::fs::File,
    from: &str,
    target: &std::fs::File,
    to: &str,
) -> Result<()> {
    let (source, from) = resource_location(source, from, false)?;
    let (target, to) = resource_location(target, to, true)?;
    rustix::fs::renameat(&source, from, &target, to).map_err(layout_error)?;
    source.sync_all()?;
    target.sync_all()?;
    Ok(())
}

pub(crate) fn valid_id(id: &str) -> Result<()> {
    let name = package_name(id);
    if name.len() > 63
        || !name.split('/').all(|segment| {
            segment.len() <= 63
                && segment
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_lowercase)
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
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
        .is_some_and(|(publisher, _)| crate::package::is_standard_author(publisher))
        && !package_name(id).starts_with("std/")
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
        tx.execute_batch(include_str!("schema.sql"))?;
        tx.execute_batch("INSERT INTO acl.lib_capability_operation SELECT * FROM acl.pishoo_legacy_operation; DROP TABLE acl.pishoo_legacy_operation;")?;
    }

    tx.execute("UPDATE lib_capability_operation SET path='/'||lib_name||substr(path,5) WHERE substr(path,1,5)='/std/' AND lib_name IN (SELECT name FROM package WHERE has_lib=1 AND (author IS NULL OR author NOT IN ('std.dhttp.net','dev.genmeta.tech.dhttp.net')))",[])?;
    tx.execute(
        r#"
UPDATE pkg_registry.package_operation AS o
SET path='/'||lib_name||substr(path,5)
WHERE substr(path,1,5)='/std/' AND EXISTS (
 SELECT 1 FROM pkg_registry.package p
 WHERE p.name=o.lib_name AND p.version=o.version
 AND (p.author IS NULL OR p.author NOT IN ('std.dhttp.net','dev.genmeta.tech.dhttp.net'))
)
"#,
        [],
    )?;
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
    let broken: bool = tx.query_row(
        r#"
SELECT EXISTS(SELECT 1
FROM pkg_registry.package_dependency d
LEFT JOIN pkg_registry.package p ON p.name=d.dependency_name AND p.version=d.version
WHERE p.name IS NULL OR (d.expected_author IS NOT NULL AND p.author<>d.expected_author))
"#,
        [],
        |r| r.get(0),
    )?;
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
        _ => "package_current",
    };
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_list WHERE type IN ('table','view') AND name=?1)",
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
            if package_name(&id) != name
                || records
                    .iter()
                    .any(|(other, _): &(String, String)| package_name(other) == name)
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

fn check_name(conn: &rusqlite::Connection, id: &str) -> Result<()> {
    use rusqlite::OptionalExtension;
    let identity = "CASE WHEN author IS NULL THEN name ELSE author||'/'||name END";
    let other: Option<String> = conn
        .query_row(
            &format!("SELECT {identity} FROM package WHERE name=?1 AND ({identity})<>?2"),
            rusqlite::params![package_name(id), id],
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
            .is_some_and(|(publisher, _)| crate::package::is_standard_author(publisher))
        {
            "/std".to_owned()
        } else {
            format!("/{}", package_name(id))
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
        for path in package.keys().filter_map(|path| {
            path.strip_prefix(if package.contains_key("manifest.toml") {
                "web/"
            } else {
                "public/"
            })
        }) {
            let suffix = if path == "index.html" {
                String::new()
            } else {
                path.to_owned()
            };
            routes.insert(
                format!("/app/{}/{}", app_name(id), suffix),
                vec!["GET".into(), "HEAD".into()],
            );
        }
        // The canonical root's redirect is also an actual static operation.
        routes.insert(
            format!("/app/{}", app_name(id)),
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
    staged: &std::collections::HashSet<(&str, &str)>,
) -> Result<()> {
    if kind == "app" || !crate::package::is_standard_author(id.split('/').next().unwrap_or("")) {
        let mount = if kind == "app" {
            format!("/app/{}", app_name(id))
        } else {
            format!("/{}", package_name(id))
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
            let occupied = if kind == "app" {
                path.strip_prefix("/app/")
                    .and_then(|tail| app_mount(id, tail))
                    .is_some()
            } else {
                path == mount
                    || path
                        .strip_prefix(&mount)
                        .is_some_and(|tail| tail.starts_with('/'))
            };
            if occupied {
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("{kind} {mount} conflicts with configured proxy {location}"),
                )));
            }
        }
    }
    let routes = package_routes(package, id, kind)?;
    let versions = conn
        .prepare(
            r#"
SELECT CASE WHEN p.author IS NULL THEN p.name ELSE p.author||'/'||p.name END,p.version
FROM pkg_registry.package p
WHERE ?1 IS NULL OR (CASE WHEN p.author IS NULL THEN p.name ELSE p.author||'/'||p.name END)=?1
ORDER BY p.name,p.version
"#,
        )?
        .query_map([None::<&str>], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (other, version) in versions {
        if other == id || staged.contains(&(other.as_str(), version.as_str())) {
            continue;
        }
        let metadata = version_metadata(conn, &other, Some(&version))?;
        if kind == "app" && metadata["has_app"] == true && app_name(id) == app_name(&other) {
            return Err(Error::BadRequest(format!(
                "App mount /app/{} is already in use",
                app_name(id)
            )));
        }
        for other_kind in ["lib", "app"] {
            if metadata[format!("has_{other_kind}")] != true {
                continue;
            }
            let root = child_dir(&profile_dir(profile)?, std::ffi::OsStr::new(other_kind))?;
            for (path, methods) in installed_routes(&root, &other, other_kind, &metadata)? {
                if let Some((_, ours)) = routes
                    .iter()
                    .find(|(ours, _)| super::api_paths_overlap(ours, &path))
                    && ((kind == "lib" && other_kind == "lib")
                        || methods.iter().any(|method| ours.contains(method)))
                {
                    return Err(Error::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!(
                            "route {path} conflicts with installed {other_kind} {other} {version}"
                        ),
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
    metadata: &serde_json::Value,
) -> Result<std::collections::BTreeMap<String, Vec<String>>> {
    if kind == "lib" {
        let wasm = registered_wasm(root, id, metadata)?;
        return package_routes(
            &std::collections::BTreeMap::from([("backend.wasm".into(), wasm)]),
            id,
            kind,
        );
    }
    let directory = installed_app_directory(root, id, metadata)?;
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

fn save_package_identity(
    conn: &rusqlite::Transaction<'_>,
    id: &str,
    api: &serde_json::Value,
) -> Result<()> {
    check_name(conn, id)?;
    let name = package_name(id);
    let author = id.split_once('/').map(|(author, _)| author);
    let version = api["version"].as_str().ok_or_else(expanded_error)?;
    let previous_owner: Option<Option<String>> = conn
        .query_row("SELECT author_owner_hash FROM pkg_registry.package WHERE name=?1 ORDER BY installed_at LIMIT 1", [name], |r| r.get(0))
        .optional()?;
    if let Some(Some(owner)) = previous_owner
        && api["author_owner_hash"].as_str() != Some(&owner)
    {
        return Err(super::signing::policy(
            "publisher_ownership_conflict",
            http::StatusCode::CONFLICT,
        ));
    }
    match version_metadata(conn, id, Some(version)) {
        Ok(current)
            if !current["package_sha256"].is_null()
                && current["package_sha256"] != api["package_sha256"] =>
        {
            return Err(super::signing::policy(
                "package_content_conflict",
                http::StatusCode::CONFLICT,
            ));
        }
        Ok(_) | Err(Error::RouteNotFound) => {}
        Err(error) => return Err(error),
    }
    conn.execute(
        r#"
INSERT INTO pkg_registry.package(name,author,version,title,description,license,repository,installed_at,upgraded_at,has_lib,has_app,author_owner_hash,package_sha256,signer_leaf_fingerprint_sha256) VALUES(?1,?2,?3,?4,?5,?6,?7,unixepoch(),NULL,?8,?9,?10,?11,?12) ON CONFLICT(name,version) DO UPDATE
SET author=excluded.author,title=excluded.title,description=excluded.description,license=excluded.license,repository=excluded.repository,upgraded_at=unixepoch(),has_lib=excluded.has_lib,has_app=excluded.has_app,author_owner_hash=excluded.author_owner_hash,package_sha256=excluded.package_sha256,signer_leaf_fingerprint_sha256=excluded.signer_leaf_fingerprint_sha256
"#,
        rusqlite::params![
            name,
            author,
            version,
            api["title"].as_str(),
            api["description"].as_str(),
            api["license"].as_str(),
            api["repository"].as_str(),
            api["has_lib"].as_bool().unwrap(),
            api["has_app"].as_bool().unwrap(),
            api["author_owner_hash"].as_str(),
            api["package_sha256"].as_str(),
            api["signer_leaf_fingerprint_sha256"].as_str()
        ],
    )?;
    conn.execute("INSERT INTO pkg_registry.package_active(name,version) VALUES(?1,?2) ON CONFLICT(name) DO NOTHING", rusqlite::params![name, version])?;
    Ok(())
}

fn save_package_relations(
    conn: &rusqlite::Transaction<'_>,
    id: &str,
    api: &serde_json::Value,
    package: &super::PackageFiles,
) -> Result<()> {
    let name = package_name(id);
    let author = id.split_once('/').map(|(author, _)| author);
    let version = api["version"].as_str().ok_or_else(expanded_error)?;
    conn.execute(
        "DELETE FROM pkg_registry.package_dependency WHERE package_name=?1 AND package_version=?2",
        rusqlite::params![name, version],
    )?;
    for (dependency, dependency_version) in declared_dependencies(api) {
        let (dependency_author, dependency_name) = dependency
            .split_once('/')
            .filter(|(author, _)| dhttp_home::normalize_name(author).as_deref() == Some(*author))
            .map_or((None, dependency.as_str()), |(author, name)| {
                (Some(author), name)
            });
        let matches: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pkg_registry.package WHERE (?1 IS NULL OR author=?1) AND name=?2 AND version=?3)",
            rusqlite::params![dependency_author, dependency_name, dependency_version],
            |r| r.get(0),
        )?;
        if !matches {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("dependency {dependency} {dependency_version} is not installed"),
            )));
        }
        conn.execute(
            "INSERT INTO pkg_registry.package_dependency(package_name,package_version,dependency_name,version,expected_author) VALUES(?1,?2,?3,?4,?5)",
            rusqlite::params![
                name,
                version,
                dependency_name,
                dependency_version,
                dependency_author
            ],
        )?;
    }
    let mut groups = if api["has_lib"] == true {
        capability_rows(package)?
    } else {
        Vec::new()
    };
    if !groups.is_empty() {
        let prefix = if author.is_some_and(crate::package::is_standard_author) {
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
                endpoints.push(("GET".into(), format!("/app/{}", app_name(id))));
            }
        }
    }
    conn.execute(
        "DELETE FROM pkg_registry.package_capability WHERE lib_name=?1 AND version=?2",
        rusqlite::params![name, version],
    )?;
    for (capability, title, endpoints) in &groups {
        conn.execute(
            "INSERT INTO pkg_registry.package_capability VALUES(?1,?2,?3,?4)",
            rusqlite::params![name, version, capability, title],
        )?;
        for (method, path) in endpoints {
            conn.execute(
                "INSERT INTO pkg_registry.package_operation VALUES(?1,?2,?3,?4,?5)",
                rusqlite::params![name, version, capability, method, path],
            )?;
        }
    }
    refresh_capabilities(conn, name)
}

fn refresh_capabilities(conn: &rusqlite::Transaction<'_>, name: &str) -> Result<()> {
    let mut groups = std::collections::BTreeMap::new();
    for row in conn
        .prepare(
            r#"
SELECT c.capability_id,c.title
FROM pkg_registry.package_capability c
LEFT JOIN pkg_registry.package_active a ON a.name=c.lib_name
WHERE c.lib_name=?1
ORDER BY c.version=a.version DESC,c.version DESC,c.capability_id
"#,
        )?
        .query_map([name], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
    {
        let (id, title) = row?;
        groups.entry(id).or_insert((title, Vec::new()));
    }
    let mut seen = std::collections::HashSet::new();
    for row in conn
        .prepare(
            r#"
SELECT o.capability_id,o.method,o.path
FROM pkg_registry.package_operation o
LEFT JOIN pkg_registry.package_active a ON a.name=o.lib_name
WHERE o.lib_name=?1
ORDER BY o.version=a.version DESC,o.version DESC,o.capability_id,o.method,o.path
"#,
        )?
        .query_map([name], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
    {
        let (id, method, path) = row?;
        if seen.insert((method.clone(), path.clone())) {
            groups
                .get_mut(&id)
                .ok_or_else(expanded_error)?
                .1
                .push((method, path));
        }
    }
    let groups = groups
        .into_iter()
        .map(|(id, (title, endpoints))| (id, title, endpoints))
        .collect::<Vec<_>>();
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
                dep["publisher"].as_str().map_or_else(
                    || dep["name"].as_str().unwrap().to_owned(),
                    |author| format!("{author}/{}", dep["name"].as_str().unwrap()),
                ),
                dep["version"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
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

fn versioned_wasm_name(id: &str, metadata: &serde_json::Value) -> Result<String> {
    valid_id(id)?;
    let version = metadata["version"].as_str().ok_or_else(expanded_error)?;
    validate_resource_version(version)?;
    Ok(format!("{}.wasm.{version}", package_name(id)))
}

fn validate_resource_version(version: &str) -> Result<()> {
    if version.is_empty()
        || version.len() > 128
        || !version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+'))
    {
        return Err(expanded_error());
    }
    Ok(())
}

// Only a missing exact version may fall back to the previous unversioned layout.
fn installed_wasm_name(
    parent: &std::fs::File,
    id: &str,
    metadata: &serde_json::Value,
) -> Result<String> {
    let versioned = versioned_wasm_name(id, metadata)?;
    let version = metadata["version"].as_str().ok_or_else(expanded_error)?;
    for path in std::iter::once(versioned.clone()).chain(legacy_wasm_names(id, version)) {
        match resource_stat(parent, &path) {
            Ok(_) => return Ok(path),
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(versioned)
}

fn legacy_wasm_names(id: &str, version: &str) -> [String; 5] {
    let name = package_name(id);
    let encoded = name.replace('/', "%2F");
    [
        format!("{encoded}.wasm.{version}"),
        format!("{name}_{version}.wasm"),
        format!("{encoded}_{version}.wasm"),
        format!("{name}.wasm"),
        format!("{encoded}.wasm"),
    ]
}

fn versioned_app_name(id: &str, metadata: &serde_json::Value) -> Result<String> {
    versioned_wasm_name(id, metadata)?;
    Ok(format!(
        "{}.{}",
        app_name(id),
        metadata["version"].as_str().ok_or_else(expanded_error)?
    ))
}

fn installed_app_directory(
    root: &std::fs::File,
    id: &str,
    metadata: &serde_json::Value,
) -> Result<std::fs::File> {
    let leaf = installed_app_leaf(root, id, metadata)?;
    resource_directory(root, &leaf, false)
}

fn installed_app_leaf(
    root: &std::fs::File,
    id: &str,
    metadata: &serde_json::Value,
) -> Result<String> {
    let canonical = versioned_app_name(id, metadata)?;
    let version = metadata["version"].as_str().ok_or_else(expanded_error)?;
    let previous = format!("{}.{version}", package_name(id));
    for path in [&canonical, &previous] {
        match resource_stat(root, path) {
            Ok(stat)
                if rustix::fs::FileType::from_raw_mode(stat.st_mode)
                    == rustix::fs::FileType::Directory =>
            {
                return Ok(path.clone());
            }
            Ok(_) => return Err(expanded_error()),
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    for path in [
        app_name(id).to_owned(),
        package_name(id).to_owned(),
        package_name(id).replace('/', "%2F"),
    ] {
        match resource_stat(root, &path) {
            Ok(stat)
                if rustix::fs::FileType::from_raw_mode(stat.st_mode)
                    == rustix::fs::FileType::Directory =>
            {
                return Ok(path);
            }
            Ok(_) => {}
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(canonical)
}

fn write_expanded(
    dir: &std::fs::File,
    package: &super::PackageFiles,
    id: &str,
    kind: &str,
) -> Result<()> {
    if kind == "lib" {
        write_component(
            dir,
            &versioned_wasm_name(id, &package_metadata(package)?)?,
            super::format::waz_backend(package).ok_or_else(expanded_error)?,
        )?;
    } else {
        for (path, data) in package {
            let Some(relative) = path.strip_prefix(if package.contains_key("manifest.toml") {
                "web/"
            } else {
                "public/"
            }) else {
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

pub(crate) fn disk_version_bytes(
    profile: &IdentityProfile,
    root: &std::fs::File,
    id: &str,
    version: Option<&str>,
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
    let metadata = version_metadata(&conn, id, version)?;
    if metadata["has_lib"] != true {
        return Err(Error::RouteNotFound);
    }
    registered_wasm(&locked, id, &metadata)
}

// The caller holds the publication lock for the supplied registration snapshot.
pub(crate) fn registered_wasm(
    root: &std::fs::File,
    id: &str,
    metadata: &serde_json::Value,
) -> Result<Vec<u8>> {
    valid_id(id)?;
    let parent = resource_parent(root, id, false)?;
    let file = expanded_file(&parent, &installed_wasm_name(&parent, id, metadata)?)?;
    read_runtime_wasm(file)
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

const REGISTRATION_FACTS: &str = r#"
-- Package registration schema v4: snapshot used by installation/recovery journals.
SELECT json_object(
 'package',json((SELECT json_array(name,author,version,title,description,license,repository,installed_at,upgraded_at,has_lib,has_app,author_owner_hash,package_sha256,signer_leaf_fingerprint_sha256) FROM package WHERE name=?1)),
 'dependencies',json((SELECT json_group_array(json_array(dependency_name,version)) FROM (SELECT dependency_name,version FROM package_dependency WHERE package_name=?1 ORDER BY dependency_name))),
 'capabilities',json((SELECT json_group_array(json_array(capability_id,title)) FROM (SELECT capability_id,title FROM lib_capability WHERE lib_name=?1 ORDER BY capability_id))),
 'operations',json((SELECT json_group_array(json_array(capability_id,method,path)) FROM (SELECT capability_id,method,path FROM lib_capability_operation WHERE lib_name=?1 ORDER BY capability_id,method,path)))
)
"#;
const REGISTRATION_FACTS_V3: &str = r#"
-- Package registration schema v3: snapshot used by installation/recovery journals.
SELECT json_object(
 'package',json((SELECT json_array(name,author,version,title,description,license,repository,installed_at,upgraded_at,has_lib,has_app) FROM package WHERE name=?1)),
 'dependencies',json((SELECT json_group_array(json_array(dependency_name,version)) FROM (SELECT dependency_name,version FROM package_dependency WHERE package_name=?1 ORDER BY dependency_name))),
 'capabilities',json((SELECT json_group_array(json_array(capability_id,title)) FROM (SELECT capability_id,title FROM lib_capability WHERE lib_name=?1 ORDER BY capability_id))),
 'operations',json((SELECT json_group_array(json_array(capability_id,method,path)) FROM (SELECT capability_id,method,path FROM lib_capability_operation WHERE lib_name=?1 ORDER BY capability_id,method,path)))
)
"#;

pub(crate) fn registration_snapshot(
    conn: &rusqlite::Connection,
    name: &str,
    signing_evidence: bool,
) -> Result<String> {
    let query = if signing_evidence {
        REGISTRATION_FACTS
    } else {
        REGISTRATION_FACTS_V3
    };
    Ok(conn.query_row(query, [name], |row| row.get(0))?)
}

pub(crate) fn has_signing_evidence(conn: &rusqlite::Connection) -> Result<bool> {
    Ok(conn.query_row(
        r#"
SELECT EXISTS(
 SELECT 1 FROM pragma_table_info('package','pkg_registry')
 WHERE name='signer_leaf_fingerprint_sha256'
)
"#,
        [],
        |row| row.get(0),
    )?)
}

fn registered_metadata(
    conn: &rusqlite::Connection,
    _kind: &str,
    id: &str,
) -> Result<serde_json::Value> {
    version_metadata(conn, id, None)
}

fn version_metadata(
    conn: &rusqlite::Connection,
    id: &str,
    version: Option<&str>,
) -> Result<serde_json::Value> {
    let mut value = conn.query_row(r#"
SELECT name,author,version,title,description,license,repository,installed_at,upgraded_at,has_lib,has_app,author_owner_hash,package_sha256,signer_leaf_fingerprint_sha256
FROM pkg_registry.package
WHERE (CASE WHEN author IS NULL THEN name ELSE author||'/'||name END)=?1 AND version=coalesce(?2,(SELECT version
FROM pkg_registry.package_active
WHERE name=pkg_registry.package.name))
"#, rusqlite::params![id, version], |r| {
        let name: String = r.get(0)?;
        Ok(serde_json::json!({"id":id,"name":name,"author":r.get::<_,Option<String>>(1)?,"version":r.get::<_,String>(2)?,
            "title":r.get::<_,Option<String>>(3)?.unwrap_or(name),"description":r.get::<_,Option<String>>(4)?,"license":r.get::<_,Option<String>>(5)?,"repository":r.get::<_,Option<String>>(6)?,
            "installed_at":r.get::<_,Option<i64>>(7)?,"upgraded_at":r.get::<_,Option<i64>>(8)?,"has_lib":r.get::<_,bool>(9)?,"has_app":r.get::<_,bool>(10)?,"author_owner_hash":r.get::<_,Option<String>>(11)?,"package_sha256":r.get::<_,Option<String>>(12)?,"signer_leaf_fingerprint_sha256":r.get::<_,Option<String>>(13)?,"endpoints":[]}))
    }).optional()?.ok_or(Error::RouteNotFound)?;
    let dependencies = conn.prepare(r#"
SELECT p.author,p.name,d.version
FROM pkg_registry.package_dependency d
JOIN pkg_registry.package p ON p.name=d.dependency_name AND p.version=d.version
WHERE d.package_name=?1 AND d.package_version=?2
ORDER BY p.author,p.name
"#)?
        .query_map(rusqlite::params![package_name(id), value["version"].as_str()], |r| Ok(serde_json::json!({"publisher":r.get::<_,Option<String>>(0)?,"name":r.get::<_,String>(1)?,"version":r.get::<_,String>(2)?})))?
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
    for value in &mut values {
        let id = value["id"].as_str().ok_or_else(expanded_error)?;
        let versions = tx
            .prepare(
                r#"
SELECT CASE WHEN p.author IS NULL THEN p.name ELSE p.author||'/'||p.name END,p.version
FROM pkg_registry.package p
WHERE ?1 IS NULL OR (CASE WHEN p.author IS NULL THEN p.name ELSE p.author||'/'||p.name END)=?1
ORDER BY p.name,p.version
"#,
            )?
            .query_map([Some(id)], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        value["installed_versions"] = serde_json::json!(versions);
        value["active_version"] = value["version"].clone();
    }
    if id.is_some() {
        package_documents(profile, values.pop().ok_or(Error::RouteNotFound)?)
    } else {
        Ok(values.into())
    }
}

/// Only facts needed to load runtime Libs, including inactive exact dependencies.
pub(crate) fn runtime_libs(profile: &IdentityProfile) -> Result<Vec<serde_json::Value>> {
    let database = installation_database(profile, false)?;
    let mut statement = database.prepare(r#"
SELECT p.author,p.name,p.version,p.author_owner_hash,
       EXISTS(SELECT 1 FROM pkg_registry.package_active a WHERE a.name=p.name AND a.version=p.version),
       json_group_array(json_object('publisher',t.author,'name',t.name,'version',t.version))
           FILTER (WHERE t.name IS NOT NULL)
FROM pkg_registry.package p
LEFT JOIN pkg_registry.package_dependency d ON d.package_name=p.name AND d.package_version=p.version
LEFT JOIN pkg_registry.package t ON t.name=d.dependency_name AND t.version=d.version
WHERE p.has_lib=1
GROUP BY p.name,p.version
"#)?;
    let rows = statement.query_map([], |row| {
        let author: Option<String> = row.get(0)?;
        let name: String = row.get(1)?;
        let id = author.map_or_else(|| name.clone(), |a| format!("{a}/{name}"));
        let dependencies: serde_json::Value = serde_json::from_str(&row.get::<_, String>(5)?)
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    5,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?;
        Ok(
            serde_json::json!({"id":id,"name":name,"version":row.get::<_,String>(2)?,
            "author_owner_hash":row.get::<_,Option<String>>(3)?, "active":row.get::<_,bool>(4)?,
            "dependencies":dependencies}),
        )
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(crate) fn installed_package_versions(
    profile: &IdentityProfile,
    id: Option<&str>,
) -> Result<Vec<serde_json::Value>> {
    let database = installation_database(profile, false)?;
    let rows = database
        .prepare(
            r#"
SELECT CASE WHEN p.author IS NULL THEN p.name ELSE p.author||'/'||p.name END,p.version
FROM pkg_registry.package p
WHERE ?1 IS NULL OR (CASE WHEN p.author IS NULL THEN p.name ELSE p.author||'/'||p.name END)=?1
ORDER BY p.name,p.version
"#,
        )?
        .query_map([id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut versions = Vec::new();
    for (id, version) in rows {
        versions.push(version_metadata(&database, &id, Some(&version))?);
    }
    Ok(versions)
}

pub(crate) fn installed_version(
    profile: &IdentityProfile,
    id: &str,
    version: &str,
) -> Result<serde_json::Value> {
    let database = installation_database(profile, false)?;
    let mut value = version_metadata(&database, id, Some(version))?;
    let active: Option<String> = database
        .query_row(
            "SELECT version FROM pkg_registry.package_active WHERE name=?1",
            [package_name(id)],
            |row| row.get(0),
        )
        .optional()?;
    value["active_version"] = serde_json::json!(active);
    let dependents = database.prepare(r#"
SELECT package_name,package_version
FROM pkg_registry.package_dependency
WHERE dependency_name=?1 AND (?2 IS NULL OR version=?2)
ORDER BY package_name,package_version
"#)?.query_map(rusqlite::params![package_name(id), version], |row| {
        Ok(serde_json::json!({"name": row.get::<_, String>(0)?, "version": row.get::<_, String>(1)?}))
    })?.collect::<rusqlite::Result<Vec<_>>>()?;
    value["dependents"] = dependents.into();
    package_documents(profile, value)
}

fn package_documents(
    profile: &IdentityProfile,
    mut value: serde_json::Value,
) -> Result<serde_json::Value> {
    if let Some(hash) = value["package_sha256"].as_str()
        && let Some(files) = cached_release(profile, &value, hash)?
    {
        let manifest = package_metadata(&files)?;
        if let Some(docs) = manifest.get("docs") {
            value["docs"] = docs.clone();
        }
        if let Some(manual) = files.get("docs/manual.md") {
            value["manual"] = std::str::from_utf8(manual)
                .map_err(|_| expanded_error())?
                .into();
        }
    }
    Ok(value)
}

fn release_cache_path(name: &str, version: &str) -> Result<String> {
    valid_id(name)?;
    if package_name(name) != name {
        return Err(expanded_error());
    }
    validate_resource_version(version)?;
    // The registry database and the old cache already occupy these root names.
    match name.split_once('/') {
        Some((prefix @ ("database" | "releases"), rest)) => {
            Ok(format!("{prefix}~/{rest}-{version}.waz"))
        }
        _ => Ok(format!("{name}-{version}.waz")),
    }
}

fn release_archive_name(metadata: &serde_json::Value) -> Result<String> {
    release_cache_path(
        metadata["name"].as_str().ok_or_else(expanded_error)?,
        metadata["version"].as_str().ok_or_else(expanded_error)?,
    )
}

fn legacy_release_name(hash: &str) -> Result<String> {
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(expanded_error());
    }
    Ok(format!("releases/{hash}.waz"))
}

fn read_legacy_archive(
    directory: &std::fs::File,
    name: &str,
    version: &str,
    legacy_hash_path: &str,
    hash: &str,
) -> Result<Option<Vec<u8>>> {
    let named = format!("releases/{name}-{version}.waz");
    match read_release_bytes(directory, &named, hash)? {
        Some(bytes) => Ok(Some(bytes)),
        None => read_release_bytes(directory, legacy_hash_path, hash),
    }
}

fn read_release_bytes(
    directory: &std::fs::File,
    path: &str,
    hash: &str,
) -> Result<Option<Vec<u8>>> {
    use std::io::Read;

    use sha2::{Digest, Sha256};

    let file = match expanded_file(directory, path) {
        Ok(file) => file,
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    file.take(64 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 64 * 1024 * 1024 || format!("{:x}", Sha256::digest(&bytes)) != hash {
        return Err(expanded_error());
    }
    Ok(Some(bytes))
}

fn cached_release(
    profile: &IdentityProfile,
    metadata: &serde_json::Value,
    hash: &str,
) -> Result<Option<super::PackageFiles>> {
    let legacy = legacy_release_name(hash)?;
    let directory = match resource_directory(&profile_dir(profile)?, ".volume/sys/pkg", false) {
        Ok(directory) => directory,
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let bytes = match read_release_bytes(&directory, &release_archive_name(metadata)?, hash)? {
        Some(bytes) => Some(bytes),
        None => read_legacy_archive(
            &directory,
            metadata["name"].as_str().ok_or_else(expanded_error)?,
            metadata["version"].as_str().ok_or_else(expanded_error)?,
            &legacy,
            hash,
        )?,
    };
    bytes
        .map(|bytes| {
            super::format::waz_read(&bytes)
                .map_err(|error| Error::InvalidComponent(error.to_string()))
        })
        .transpose()
}

fn write_release_archive(directory: &std::fs::File, path: &str, bytes: &[u8]) -> Result<()> {
    let (parent, name) = resource_location(directory, path, true)?;
    let temporary = temporary_name()?;
    write_component(&parent, &temporary, bytes)?;
    let cleanup = scopeguard::guard((), |_| {
        let _ = rustix::fs::unlinkat(&parent, temporary.as_str(), rustix::fs::AtFlags::empty());
    });
    rustix::fs::renameat(&parent, temporary.as_str(), &parent, name).map_err(layout_error)?;
    parent.sync_all()?;
    drop(cleanup);
    Ok(())
}

fn cache_release(
    profile: &IdentityProfile,
    bytes: &[u8],
    metadata: &serde_json::Value,
) -> Result<()> {
    verify_archive_digest(bytes, metadata)?;
    let directory = resource_directory(&profile_dir(profile)?, ".volume/sys/pkg", true)?;
    write_release_archive(&directory, &release_archive_name(metadata)?, bytes)
}

fn verify_archive_digest(bytes: &[u8], metadata: &serde_json::Value) -> Result<()> {
    use sha2::{Digest, Sha256};

    let hash = format!("{:x}", Sha256::digest(bytes));
    if metadata["package_sha256"].as_str() != Some(hash.as_str()) {
        return Err(expanded_error());
    }
    Ok(())
}

// The identity lock covers both the release cache and publication journals.
fn collect_release_cache(parent: &std::fs::File, conn: &rusqlite::Connection) -> Result<()> {
    let identity = cap_std::fs::Dir::from_std_file(parent.try_clone()?);
    for entry in identity.entries()? {
        let entry = entry?;
        let name = entry.file_name();
        if name.as_encoded_bytes().starts_with(b".pishoo-") && entry.file_type()?.is_dir() {
            let marker = std::path::Path::new(&name).join(".transaction.json");
            match identity.symlink_metadata(marker) {
                // Pending recovery may still need either set of original bytes.
                Ok(_) => return Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    let directory = match resource_directory(parent, ".volume/sys/pkg", false) {
        Ok(directory) => directory,
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let mut referenced = std::collections::HashSet::new();
    let mut statement = conn.prepare(
        "SELECT name, version, package_sha256 FROM pkg_registry.package WHERE package_sha256 IS NOT NULL",
    )?;
    let packages = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    for package in packages {
        let (name, version, hash) = package?;
        let path = release_cache_path(&name, &version)?;
        let legacy = legacy_release_name(&hash)?;
        if let Some(bytes) = read_legacy_archive(&directory, &name, &version, &legacy, &hash)?
            && read_release_bytes(&directory, &path, &hash)?.is_none()
        {
            write_release_archive(&directory, &path, &bytes)?;
        }
        referenced.insert(path);
    }
    sweep_release_archives(&directory, "", &referenced)?;
    Ok(())
}

fn sweep_release_archives(
    root: &std::fs::File,
    prefix: &str,
    referenced: &std::collections::HashSet<String>,
) -> Result<bool> {
    let directory = resource_directory(root, prefix, false)?;
    let entries = cap_std::fs::Dir::from_std_file(directory.try_clone()?);
    let mut removed = false;
    for entry in entries.entries()? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str().filter(|name| !name.starts_with('.')) else {
            continue;
        };
        let path = if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}/{name}")
        };
        let kind = entry.file_type()?;
        if kind.is_dir() {
            let package_path =
                path.replacen("database~/", "database/", 1)
                    .replacen("releases~/", "releases/", 1);
            if (matches!(path.as_str(), "database~" | "releases~")
                || valid_id(&package_path).is_ok())
                && sweep_release_archives(root, &path, referenced)?
                && (path == "releases" || path.starts_with("releases/"))
            {
                entries.remove_dir(name)?;
                removed = true;
            }
        } else if kind.is_file() && name.ends_with(".waz") && !referenced.contains(&path) {
            entries.remove_file(name)?;
            removed = true;
        }
    }
    if removed {
        directory.sync_all()?;
    }
    Ok(entries.entries()?.next().is_none())
}

fn preview_package_versions(parent: &std::fs::File) -> Result<Vec<(String, String)>> {
    use std::io::Read;
    let lib = match child_dir(parent, std::ffi::OsStr::new("lib")) {
        Ok(lib) => lib,
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Vec::new());
        }
        Err(error) => return Err(error),
    };
    let device = lib.metadata()?.dev();
    let mut pending = vec![lib];
    let mut versions = Vec::new();
    while let Some(root) = pending.pop() {
        let directory = cap_std::fs::Dir::from_std_file(root.try_clone()?);
        for entry in directory.entries()? {
            let entry = entry?;
            let name = entry.file_name();
            if !entry.file_type()?.is_dir() || name.as_encoded_bytes().starts_with(b".") {
                continue;
            }
            let child = child_dir(&root, &name)?;
            if child.metadata()?.dev() != device {
                continue;
            }
            if !name.as_encoded_bytes().ends_with(b".preview") {
                pending.push(child);
                continue;
            }
            let manifest = match expanded_file(&child, "manifest.json") {
                Ok(manifest) => manifest,
                Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            let manifest: serde_json::Value =
                serde_json::from_reader(manifest.take(1024 * 1024))
                    .map_err(|error| Error::InvalidConfig(error.to_string()))?;
            let dependencies = manifest["dependencies"].as_array().into_iter().flatten();
            for item in std::iter::once(&manifest["package"]).chain(dependencies) {
                if let (Some(name), Some(version)) =
                    (item["name"].as_str(), item["version"].as_str())
                {
                    versions.push((name.to_owned(), version.to_owned()));
                }
            }
        }
    }
    Ok(versions)
}

fn retire_unused_versions(
    tx: &rusqlite::Transaction<'_>,
    parent: &std::fs::File,
) -> Result<Vec<(String, String)>> {
    let previews = serde_json::to_string(&preview_package_versions(parent)?)
        .map_err(|error| Error::InvalidConfig(error.to_string()))?;
    let retired = tx
        .prepare(
            r#"
WITH RECURSIVE retained(name, version) AS (
    SELECT name, version FROM pkg_registry.package_active
    UNION
    SELECT name, version FROM pkg_registry.package_root
    UNION
    SELECT value ->> 0, value ->> 1 FROM json_each(?1)
    UNION
    SELECT d.dependency_name, d.version
    FROM retained r
    JOIN pkg_registry.package_dependency d ON d.package_name=r.name AND d.package_version=r.version
)
SELECT CASE WHEN p.author IS NULL THEN p.name ELSE p.author||'/'||p.name END, p.version
FROM pkg_registry.package p
WHERE NOT EXISTS (SELECT 1 FROM retained r WHERE r.name=p.name AND r.version=p.version)
ORDER BY p.name, p.version
"#,
        )?
        .query_map([previews], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // Drop all outgoing edges before deleting any version, including cycles.
    for (id, version) in &retired {
        tx.execute(
            "DELETE FROM pkg_registry.package_dependency WHERE package_name=?1 AND package_version=?2",
            rusqlite::params![package_name(id), version],
        )?;
    }
    let mut names = std::collections::BTreeSet::new();
    for (id, version) in &retired {
        let name = package_name(id);
        tx.execute(
            "DELETE FROM pkg_registry.package WHERE name=?1 AND version=?2",
            rusqlite::params![name, version],
        )?;
        names.insert(name);
    }
    for name in names {
        refresh_capabilities(tx, name)?;
    }
    Ok(retired)
}

fn registered_versions(conn: &rusqlite::Connection) -> Result<Vec<(String, String)>> {
    let mut statement = conn.prepare(
        "SELECT CASE WHEN author IS NULL THEN name ELSE author||'/'||name END, version
         FROM pkg_registry.package ORDER BY name,version",
    )?;
    let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn lock_package_writer(profile: &IdentityProfile) -> Result<std::fs::File> {
    let dir = profile_dir(profile)?;
    crate::setup::open_config(profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    rustix::fs::flock(&dir, rustix::fs::FlockOperation::LockExclusive)
        .map_err(std::io::Error::from)?;
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
    let (parent, name) = resource_location(dir, name, true)?;
    let dir = &parent;
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
    dir.sync_all()?;
    drop(scopeguard::ScopeGuard::into_inner(file));
    Ok(())
}

/// Install a verified dependency closure, with its requested
/// root last. The caller holds the existing Sandbox write lock throughout.
pub(super) fn install_packages(
    profile: &IdentityProfile,
    packages: &[(String, Vec<u8>, serde_json::Value)],
    sandbox: &mut super::Sandbox,
) -> Result<()> {
    use std::sync::Arc;

    use rustix::fs::{Mode, mkdirat};

    if packages.is_empty() || packages.len() > 100 {
        return Err(Error::BadRequest(
            "installation requires 1 to 100 packages".into(),
        ));
    }
    if sandbox.tasks.is_closed() {
        return Err(Error::Denied);
    }
    let parent = lock_package_writer(profile)?;
    let mut database = installation_database(profile, true)?;
    let tx = database.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let previous = registered_versions(&tx)?;
    let mut names = std::collections::HashSet::new();
    let mut changed = Vec::new();
    for (index, (id, bytes, proof)) in packages.iter().enumerate() {
        valid_id(id)?;
        let short = package_name(id);
        if short == "std" {
            return Err(Error::BadRequest(
                "reserved or duplicate package name".into(),
            ));
        }
        let (author, name) = id.split_once('/').ok_or_else(|| {
            super::signing::policy("signing_identity_mismatch", http::StatusCode::FORBIDDEN)
        })?;
        let version = proof["version"].as_str().ok_or_else(|| {
            super::signing::policy(
                "package_policy_upgrade_required",
                http::StatusCode::UNPROCESSABLE_ENTITY,
            )
        })?;
        if !names.insert((short.to_owned(), version.to_owned())) {
            return Err(Error::BadRequest("duplicate package version".into()));
        }
        let package = super::store::verify_release(bytes, proof, author, name, version)?;
        let at = proof["signature"]["verified_at"].as_i64().unwrap();
        let facts = super::signing::facts(&package, author, at)?;
        super::signing::check_revocations(
            &package,
            &profile.join("trust/code-signing"),
            &proof["revocation_list"],
            chrono::Utc::now().timestamp(),
        )?;
        let mut metadata = package_metadata(&package)?;
        metadata["author_owner_hash"] = facts["author_owner_hash"].clone();
        metadata["package_sha256"] = proof["package_sha256"].clone();
        metadata["signer_leaf_fingerprint_sha256"] = facts["leaf_fingerprint_sha256"].clone();
        check_resource_identity(&metadata, id)?;
        check_name(&tx, id)?;
        let current = match version_metadata(&tx, id, Some(version)).or_else(|error| match error {
            Error::RouteNotFound => registered_metadata(&tx, "pkg", id),
            other => Err(other),
        }) {
            Ok(current) => Some(current),
            Err(Error::RouteNotFound) => None,
            Err(error) => return Err(error),
        };
        if let Some(current) = &current {
            let original_owner = if current["author_owner_hash"].is_null() {
                let previous = &proof["_previous_release"];
                if previous["id"] != *id
                    || previous["author"] != author
                    || previous["name"] != name
                    || previous["version"] != current["version"]
                    || previous["signature"]["purpose"] != "code_signing"
                {
                    return Err(super::signing::policy(
                        "publisher_ownership_conflict",
                        http::StatusCode::CONFLICT,
                    ));
                }
                let previous_at =
                    previous["signature"]["verified_at"]
                        .as_i64()
                        .ok_or_else(|| {
                            super::signing::policy(
                                "publisher_ownership_conflict",
                                http::StatusCode::CONFLICT,
                            )
                        })?;
                super::signing::verify_receipt(previous, previous_at)?;
                &previous["author_owner_hash"]
            } else {
                &current["author_owner_hash"]
            };
            if original_owner != &metadata["author_owner_hash"] {
                return Err(super::signing::policy(
                    "publisher_ownership_conflict",
                    http::StatusCode::CONFLICT,
                ));
            }
            if current["version"] == metadata["version"]
                && !current["package_sha256"].is_null()
                && current["package_sha256"] != metadata["package_sha256"]
            {
                return Err(super::signing::policy(
                    "package_version_conflict",
                    http::StatusCode::CONFLICT,
                ));
            }
        }
        let same_version = current
            .as_ref()
            .is_some_and(|v| v["version"] == metadata["version"]);
        let reused = index + 1 != packages.len()
            && same_version
            && current.as_ref().is_some_and(|v| {
                v["package_sha256"] == metadata["package_sha256"]
                    && v["signer_leaf_fingerprint_sha256"]
                        == metadata["signer_leaf_fingerprint_sha256"]
                    && v["has_lib"] == metadata["has_lib"]
                    && v["has_app"] == metadata["has_app"]
            })
            && (metadata["has_lib"] != true
                || sandbox
                    .versions
                    .get(&(id.clone(), version.to_owned()))
                    .is_some_and(|lib| {
                        Some(lib.openapi.info.version.as_str()) == metadata["version"].as_str()
                    }))
            && (metadata["has_app"] != true
                || child_dir(&parent, std::ffi::OsStr::new("app"))
                    .and_then(|root| installed_app_directory(&root, id, &metadata))
                    .is_ok());
        if reused {
            cache_release(profile, bytes, &metadata)?;
            continue;
        }
        changed.push((id, bytes, package, metadata));
    }
    // Temporary rename compatibility retires verified same-owner predecessors
    // inside this transaction, before the ordinary route/dependency checks.
    let (mut retired, renamed_roots, renamed_active) = super::renames::retire(&tx, packages)?;
    for (id, _) in &retired {
        refresh_capabilities(&tx, package_name(id))?;
    }
    let mut providers = std::collections::BTreeMap::<String, &str>::new();
    let mut app_providers = std::collections::HashMap::new();
    for (id, _, package, metadata) in &changed {
        if metadata["has_app"] == true
            && app_providers
                .insert(app_name(id), id)
                .is_some_and(|previous| *previous != *id)
        {
            return Err(Error::BadRequest(format!(
                "App mount /app/{} is already in use",
                app_name(id)
            )));
        }
        if metadata["has_lib"] == true {
            for path in package_routes(package, id, "lib")?.keys() {
                if let Some((_, previous)) = providers.iter().find(|(existing, previous)| {
                    super::api_paths_overlap(existing, path) && **previous != id.as_str()
                }) {
                    return Err(Error::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("route {path} conflicts between {previous} and {id}"),
                    )));
                }
                providers.insert(path.clone(), id.as_str());
            }
        }
    }
    // Register every identity before inserting edges, including cycles.
    for (id, _, _, metadata) in &changed {
        save_package_identity(&tx, id, metadata)?;
    }
    for (id, _, package, metadata) in &changed {
        save_package_relations(&tx, id, metadata, package)?;
    }
    for id in &renamed_roots {
        let version = packages.iter().find(|(key, _, _)| key == id).unwrap().2["version"]
            .as_str()
            .unwrap();
        tx.execute("INSERT INTO pkg_registry.package_root(name,version) VALUES(?1,?2) ON CONFLICT(name) DO UPDATE SET version=excluded.version", rusqlite::params![package_name(id),version])?;
    }
    for id in &renamed_active {
        let version = packages.iter().find(|(key, _, _)| key == id).unwrap().2["version"]
            .as_str()
            .unwrap();
        tx.execute("INSERT INTO pkg_registry.package_active(name,version) VALUES(?1,?2) ON CONFLICT(name) DO UPDATE SET version=excluded.version", rusqlite::params![package_name(id),version])?;
    }
    for (index, (root_id, _, root_proof)) in packages.iter().enumerate() {
        if root_proof["_requested"] != true && index + 1 != packages.len() {
            continue;
        }
        let root_version = root_proof["version"].as_str().ok_or_else(expanded_error)?;
        tx.execute("INSERT INTO pkg_registry.package_active(name,version) VALUES(?1,?2) ON CONFLICT(name) DO UPDATE SET version=excluded.version", rusqlite::params![package_name(root_id),root_version])?;
        tx.execute("INSERT INTO pkg_registry.package_root(name,version) VALUES(?1,?2) ON CONFLICT(name) DO UPDATE SET version=excluded.version", rusqlite::params![package_name(root_id),root_version])?;
    }
    retired.extend(retire_unused_versions(&tx, &parent)?);
    changed.retain(|(id, _, _, metadata)| {
        !retired
            .iter()
            .any(|(name, version)| name == *id && metadata["version"] == *version)
    });
    let staged = changed
        .iter()
        .map(|(id, _, _, metadata)| (id.as_str(), metadata["version"].as_str().unwrap()))
        .collect::<std::collections::HashSet<_>>();
    for (id, _, package, metadata) in &changed {
        for kind in ["lib", "app"] {
            if metadata[format!("has_{kind}")] == true {
                check_routes(profile, &tx, id, kind, package, &staged)?;
            }
        }
    }
    for (id, _, _, _) in &changed {
        refresh_capabilities(&tx, package_name(id))?;
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
    for (id, bytes, package, metadata) in &changed {
        let short = format!(
            "{}.{}",
            package_name(id).replace('/', "%2F"),
            metadata["version"].as_str().ok_or_else(expanded_error)?
        );
        mkdirat(&stage, short.as_str(), Mode::from_raw_mode(0o700)).map_err(layout_error)?;
        let package_stage = child_dir(&stage, std::ffi::OsStr::new(&short))?;
        inherit_owner(&package_stage, &parent)?;
        verify_archive_digest(bytes, metadata)?;
        write_component(&package_stage, "new-archive", bytes)?;
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
            write_expanded(&dir, package, id, kind)?;
            let payload = if kind == "lib" {
                expanded_file(&dir, &versioned_wasm_name(id, metadata)?)?
            } else {
                dir
            };
            if kind == "lib" {
                let wasm = read_runtime_wasm(payload)?;
                let data = super::renames::data_path(profile, id)?;
                let mut lib = crate::sandbox::Lib::load(
                    sandbox.runtime.clone(),
                    (*id).clone(),
                    &wasm,
                    &data,
                )?;
                lib.author_owner_hash = metadata["author_owner_hash"].as_str().map(str::to_owned);
                if lib.openapi.info.version
                    != metadata["version"].as_str().ok_or_else(expanded_error)?
                {
                    return Err(expanded_error());
                }
                let registered = version_metadata(&tx, id, metadata["version"].as_str())?;
                for dependency in registered["dependencies"].as_array().unwrap() {
                    let name = dependency["name"].as_str().unwrap();
                    let target = dependency["publisher"]
                        .as_str()
                        .map_or_else(|| name.to_owned(), |author| format!("{author}/{name}"));
                    lib.dependencies.insert(
                        name.to_owned(),
                        (target, dependency["version"].as_str().unwrap().to_owned()),
                    );
                }
                libs.insert(
                    (
                        (*id).clone(),
                        metadata["version"]
                            .as_str()
                            .ok_or_else(expanded_error)?
                            .to_owned(),
                    ),
                    Arc::new(lib),
                );
            } else {
                apps.insert(
                    (
                        (*id).clone(),
                        metadata["version"]
                            .as_str()
                            .ok_or_else(expanded_error)?
                            .to_owned(),
                    ),
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
    for lib in libs.values() {
        super::access_migrations::validate_dependency(lib, |key| {
            libs.get(key).or_else(|| sandbox.versions.get(key)).map(AsRef::as_ref)
        })?;
    }
    let entry_names = changed
        .iter()
        .zip(&stages)
        .map(|((id, _, _, metadata), dir)| {
            (
                id.as_str(),
                metadata["version"].as_str().unwrap(),
                format!(
                    "{}.{}",
                    package_name(id).replace('/', "%2F"),
                    metadata["version"].as_str().unwrap()
                ),
                dir,
            )
        })
        .collect::<Vec<_>>();
    let mut retired_stages = Vec::new();
    for (id, version) in &retired {
        if changed
            .iter()
            .any(|(new, _, _, meta)| *new == id && meta["version"] == *version)
        {
            continue;
        }
        let directory = format!("retired-{}", retired_stages.len());
        mkdirat(&stage, directory.as_str(), Mode::from_raw_mode(0o700)).map_err(layout_error)?;
        retired_stages.push((
            id,
            version,
            directory.clone(),
            child_dir(&stage, std::ffi::OsStr::new(&directory))?,
        ));
    }
    let entries = entry_names
        .iter()
        .map(|(id, version, directory, stage)| (*id, *version, directory.as_str(), *stage))
        .chain(
            retired_stages
                .iter()
                .map(|(id, version, directory, stage)| {
                    (id.as_str(), version.as_str(), directory.as_str(), stage)
                }),
        )
        .collect::<Vec<_>>();
    let mut active_libs = std::collections::HashMap::new();
    let mut active_apps = std::collections::HashMap::new();
    for (id, version) in registered_packages(&tx, "lib", None)? {
        if let Some(lib) = libs
            .get(&(id.clone(), version.clone()))
            .or_else(|| sandbox.versions.get(&(id.clone(), version)))
        {
            active_libs.insert(id, Arc::clone(lib));
        } else {
            return Err(Error::InvalidConfig(
                "active Lib version was not loaded".into(),
            ));
        }
    }
    for (id, version) in registered_packages(&tx, "app", None)? {
        if let Some(app) = apps.get(&(id.clone(), version.clone())).or_else(|| {
            sandbox
                .apps
                .get(&id)
                .filter(|(current, _)| current == &version)
        }) {
            active_apps.insert(id, app.clone());
        } else {
            let root = child_dir(&parent, std::ffi::OsStr::new("app"))?;
            let metadata = version_metadata(&tx, &id, Some(&version))?;
            let directory = installed_app_directory(&root, &id, &metadata)?;
            active_apps.insert(id, (version, Arc::new(directory)));
        }
    }
    // Recheck cached evidence locally after compilation; never perform network I/O under transaction locks.
    for (_, bytes, proof) in packages {
        let at = proof["signature"]["verified_at"].as_i64().unwrap();
        let package = super::format::read_package_at(bytes, at)?;
        super::signing::check_revocations(
            &package,
            &profile.join("trust/code-signing"),
            &proof["revocation_list"],
            chrono::Utc::now().timestamp(),
        )?;
    }
    scopeguard::ScopeGuard::into_inner(cleanup);
    commit_packages_files(profile, &entries, &previous, &parent, &stage, &name, tx)?;
    for key in &retired {
        if let Some(old) = sandbox.versions.remove(key) {
            old.stream_retired.cancel();
        }
    }
    for (key, lib) in libs {
        if let Some(old) = sandbox.versions.insert(key, lib) {
            old.stream_retired.cancel();
        }
    }
    sandbox.libs = active_libs;
    sandbox.apps = active_apps;
    Ok(())
}

/// Reactivate an already verified installed release using its retained original bytes.
pub(super) fn activate_installed_version(
    profile: &IdentityProfile,
    id: &str,
    version: &str,
    sandbox: &mut super::Sandbox,
) -> Result<Option<serde_json::Value>> {
    use std::sync::Arc;

    use rustix::fs::{Mode, mkdirat};
    let parent = lock_package_writer(profile)?;
    let mut database = installation_database(profile, true)?;
    let tx = database.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let metadata = match version_metadata(&tx, id, Some(version)) {
        Ok(metadata) => metadata,
        Err(Error::RouteNotFound) => return Ok(None),
        Err(error) => return Err(error),
    };
    let Some(hash) = metadata["package_sha256"].as_str() else {
        return Ok(None);
    };
    let Some(package) = cached_release(profile, &metadata, hash)? else {
        return Ok(None);
    };
    let document = package_metadata(&package)?;
    check_resource_identity(&document, id)?;
    if document["version"] != version {
        return Err(expanded_error());
    }
    for kind in ["lib", "app"] {
        if metadata[format!("has_{kind}")] == true {
            check_routes(
                profile,
                &tx,
                id,
                kind,
                &package,
                &std::collections::HashSet::new(),
            )?;
        }
    }
    let previous = registered_versions(&tx)?;
    let name = temporary_name()?;
    mkdirat(&parent, &name, Mode::from_raw_mode(0o700)).map_err(layout_error)?;
    let cleanup_dir = cap_std::fs::Dir::from_std_file(parent.try_clone()?);
    let cleanup = scopeguard::guard((), |_| {
        let _ = cleanup_dir.remove_dir_all(&name);
    });
    let stage = child_dir(&parent, std::ffi::OsStr::new(&name))?;
    inherit_owner(&stage, &parent)?;
    let mut app = None;
    let mut lib = None;
    for kind in ["lib", "app"] {
        if metadata[format!("has_{kind}")] != true {
            continue;
        }
        let candidate = format!("new-{kind}");
        mkdirat(&stage, candidate.as_str(), Mode::from_raw_mode(0o700)).map_err(layout_error)?;
        let directory = child_dir(&stage, std::ffi::OsStr::new(&candidate))?;
        inherit_owner(&directory, &stage)?;
        write_expanded(&directory, &package, id, kind)?;
        if kind == "app" {
            app = Some((version.to_owned(), Arc::new(directory)));
        } else if let Some(current) = sandbox.versions.get(&(id.to_owned(), version.to_owned())) {
            lib = Some(Arc::clone(current));
        } else {
            let bytes = read_runtime_wasm(expanded_file(
                &directory,
                &versioned_wasm_name(id, &metadata)?,
            )?)?;
            let data = if super::is_standard_author(id.split('/').next().unwrap_or(""))
                && !package_name(id).starts_with("std/")
            {
                format!("std/{}", package_name(id))
            } else {
                package_name(id).to_owned()
            };
            let mut loaded = crate::sandbox::Lib::load(
                sandbox.runtime.clone(),
                id.to_owned(),
                &bytes,
                &profile.join(".volume").join(data),
            )?;
            loaded.author_owner_hash = metadata["author_owner_hash"].as_str().map(str::to_owned);
            for dependency in document["dependencies"].as_array().unwrap() {
                let name = dependency["name"].as_str().unwrap();
                let version = dependency["version"].as_str().unwrap();
                let target: String = tx.query_row(
                    "SELECT CASE WHEN author IS NULL THEN name ELSE author||'/'||name END FROM pkg_registry.package WHERE name=?1 AND version=?2",
                    rusqlite::params![name, version], |row| row.get(0),
                )?;
                loaded
                    .dependencies
                    .insert(name.to_owned(), (target, version.to_owned()));
            }
            lib = Some(Arc::new(loaded));
        }
    }
    if let Some(candidate) = lib.as_deref() {
        super::access_migrations::validate_dependency(candidate, |key| {
            sandbox.versions.get(key).map(AsRef::as_ref)
        })?;
    }
    tx.execute(
        r#"
INSERT INTO pkg_registry.package_active(name,version) VALUES(?1,?2) ON CONFLICT(name) DO UPDATE
SET version=excluded.version
"#,
        rusqlite::params![package_name(id), version],
    )?;
    tx.execute(
        r#"
INSERT INTO pkg_registry.package_root(name,version) VALUES(?1,?2) ON CONFLICT(name) DO UPDATE
SET version=excluded.version
"#,
        rusqlite::params![package_name(id), version],
    )?;
    refresh_capabilities(&tx, package_name(id))?;
    let retired = retire_unused_versions(&tx, &parent)?;
    mkdirat(&stage, "retired", Mode::from_raw_mode(0o700)).map_err(layout_error)?;
    let empty = child_dir(&stage, std::ffi::OsStr::new("retired"))?;
    let mut entries = vec![(id, version, "", &stage)];
    entries.extend(
        retired
            .iter()
            .map(|(id, version)| (id.as_str(), version.as_str(), "retired", &empty)),
    );
    let result = super::lib_metadata(&package, Some(id));
    scopeguard::ScopeGuard::into_inner(cleanup);
    commit_packages_files(profile, &entries, &previous, &parent, &stage, &name, tx)?;
    for key in &retired {
        if let Some(old) = sandbox.versions.remove(key) {
            old.stream_retired.cancel();
        }
    }
    if let Some(lib) = lib {
        sandbox
            .versions
            .insert((id.to_owned(), version.to_owned()), Arc::clone(&lib));
        sandbox.libs.insert(id.to_owned(), lib);
    } else {
        sandbox.libs.remove(id);
    }
    if let Some(app) = app {
        sandbox.apps.insert(id.to_owned(), app);
    } else {
        sandbox.apps.remove(id);
    }
    Ok(Some(result))
}

fn publication_resource(
    parent: &std::fs::File,
    id: &str,
    version: &str,
    kind: &str,
    paths: (&str, &str),
    candidate: Option<&str>,
    reuse: bool,
) -> Result<serde_json::Value> {
    let (old_leaf, leaf) = paths;
    let root = publication_root(parent, id, kind)?;
    resource_location(&root, leaf, true)?;
    let old = match resource_stat(&root, old_leaf) {
        Ok(stat) => {
            let file_type = rustix::fs::FileType::from_raw_mode(stat.st_mode);
            let valid = match kind {
                "lib" | "archive" | "pkg_archive" => {
                    file_type == rustix::fs::FileType::RegularFile && stat.st_nlink == 1
                }
                "app" | "volume" => file_type == rustix::fs::FileType::Directory,
                "app_link" => matches!(
                    file_type,
                    rustix::fs::FileType::Symlink | rustix::fs::FileType::Directory
                ),
                _ => false,
            };
            if !valid {
                return Err(expanded_error());
            }
            true
        }
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };
    if leaf != old_leaf && resource_stat(&root, leaf).is_ok() {
        return Err(expanded_error());
    }
    Ok(
        serde_json::json!({"id":id,"version":version,"kind":kind,"old_leaf":old_leaf,
        "leaf":leaf,"old":old,"candidate":candidate,"reuse":reuse}),
    )
}

fn app_mount_owned_by_other(
    tx: &rusqlite::Transaction<'_>,
    id: &str,
    version: Option<&str>,
) -> Result<bool> {
    Ok(tx.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM pkg_registry.package p
           LEFT JOIN pkg_registry.package_active a USING(name, version)
           WHERE p.name IN (?1, 'std/' || ?1) AND p.name <> ?2 AND p.has_app = 1
             AND CASE WHEN ?3 IS NULL THEN a.name IS NOT NULL ELSE p.version = ?3 END
         )",
        rusqlite::params![app_name(id), package_name(id), version],
        |row| row.get(0),
    )?)
}

fn publication_root(parent: &std::fs::File, id: &str, kind: &str) -> Result<std::fs::File> {
    let root = resource_directory(
        parent,
        match kind {
            "lib" => "lib",
            "archive" => ".volume/sys/pkg/releases",
            "pkg_archive" => ".volume/sys/pkg",
            "volume" => ".volume",
            _ => "app",
        },
        true,
    )?;
    if kind == "lib" {
        resource_parent(&root, id, true)
    } else {
        Ok(root)
    }
}

fn remove_publication_resource(root: &std::fs::File, leaf: &str) -> Result<()> {
    let (parent, name) = resource_location(root, leaf, false)?;
    let stat = match rustix::fs::statat(&parent, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(rustix::io::Errno::NOENT) => return Ok(()),
        Err(error) => return Err(layout_error(error)),
    };
    let dir = cap_std::fs::Dir::from_std_file(parent);
    if rustix::fs::FileType::from_raw_mode(stat.st_mode) == rustix::fs::FileType::Directory {
        dir.remove_dir_all(name)?;
    } else {
        dir.remove_file(name)?;
    }
    dir.into_std_file().sync_all()?;
    Ok(())
}

fn prune_legacy_app_parents(parent: &std::fs::File, resources: &[serde_json::Value]) -> Result<()> {
    use rustix::{
        fs::{AtFlags, unlinkat},
        io::Errno,
    };

    for resource in resources {
        let old = resource["old_leaf"].as_str().unwrap_or("");
        if !matches!(resource["kind"].as_str(), Some("app" | "app_link"))
            || !old.starts_with("std/")
            || resource["old"] != true
        {
            continue;
        }
        let app = child_dir(parent, std::ffi::OsStr::new("app"))?;
        let mut path = old;
        while let Some((directory, _)) = path.rsplit_once('/') {
            let (root, name) = match resource_location(&app, directory, false) {
                Ok(location) => location,
                Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                    path = directory;
                    continue;
                }
                Err(error) => return Err(error),
            };
            match unlinkat(&root, name, AtFlags::REMOVEDIR) {
                Ok(()) => root.sync_all()?,
                Err(Errno::NOENT) => {}
                Err(Errno::NOTEMPTY | Errno::EXIST) => break,
                Err(error) => return Err(layout_error(error)),
            }
            path = directory;
        }
    }
    Ok(())
}

fn rollback_publication(
    parent: &std::fs::File,
    stage: &std::fs::File,
    resources: &[serde_json::Value],
) -> Result<()> {
    for (index, resource) in resources.iter().enumerate().rev() {
        let root = publication_root(
            parent,
            resource["id"].as_str().ok_or_else(expanded_error)?,
            resource["kind"].as_str().ok_or_else(expanded_error)?,
        )?;
        let leaf = resource["leaf"].as_str().ok_or_else(expanded_error)?;
        let old_leaf = resource["old_leaf"].as_str().ok_or_else(expanded_error)?;
        let backup = format!("old-{index}");
        let has_backup = resource_stat(stage, &backup).is_ok();
        if resource["reuse"] == true && !has_backup && resource_stat(&root, leaf).is_ok() {
            rename_resource(&root, leaf, &root, old_leaf)?;
        } else if has_backup {
            remove_publication_resource(&root, leaf)?;
            rename_resource(stage, &backup, &root, old_leaf)?;
        } else if resource["old"] == false
            && let Some(candidate) = resource["candidate"].as_str()
            && resource_stat(stage, candidate).is_err()
        {
            remove_publication_resource(&root, leaf)?;
        }
    }
    Ok(())
}

fn commit_packages_files(
    profile: &IdentityProfile,
    entries: &[(&str, &str, &str, &std::fs::File)],
    previous: &[(String, String)],
    parent: &std::fs::File,
    stage: &std::fs::File,
    name: &str,
    tx: rusqlite::Transaction<'_>,
) -> Result<()> {
    let current = profile_dir(profile)?;
    if current.metadata()?.dev() != parent.metadata()?.dev()
        || current.metadata()?.ino() != parent.metadata()?.ino()
    {
        return Err(Error::InvalidConfig(
            "identity directory changed during publication".into(),
        ));
    }
    let cleanup_dir = cap_std::fs::Dir::from_std_file(parent.try_clone()?);
    let cleanup = scopeguard::guard((), |_| {
        let _ = cleanup_dir.remove_dir_all(name);
    });
    let mut resources = Vec::new();
    for &(id, version, _, _) in entries {
        if version_metadata(&tx, id, Some(version)).is_ok()
            && let Some((old, leaf)) = super::renames::volume_move(id)
        {
            let root = publication_root(parent, id, "volume")?;
            if resource_stat(&root, old).is_ok() {
                resources.push(publication_resource(
                    parent,
                    id,
                    version,
                    "volume",
                    (old, leaf),
                    None,
                    true,
                )?);
            }
        }
    }
    let mut active_names = std::collections::HashSet::new();
    for &(id, version, directory, package_stage) in entries {
        let metadata = serde_json::json!({"version":version});
        let prefix = if directory.is_empty() {
            String::new()
        } else {
            format!("{directory}/")
        };
        match resource_stat(package_stage, "new-archive") {
            Ok(_) => {
                let leaf = release_cache_path(package_name(id), version)?;
                let candidate = format!("{prefix}new-archive");
                resources.push(publication_resource(
                    parent,
                    id,
                    version,
                    "pkg_archive",
                    (&leaf, &leaf),
                    Some(&candidate),
                    false,
                )?);
            }
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        for kind in ["lib", "app"] {
            if kind == "app" && app_mount_owned_by_other(&tx, id, Some(version))? {
                continue;
            }
            let root = publication_root(parent, id, kind)?;
            let leaf = if kind == "lib" {
                versioned_wasm_name(id, &metadata)?
            } else {
                versioned_app_name(id, &metadata)?
            };
            if kind == "lib" {
                for legacy in [
                    format!("{}.wasm", package_name(id)),
                    format!("{}.wasm", package_name(id).replace('/', "%2F")),
                ] {
                    match resource_stat(&root, &legacy) {
                        Ok(stat)
                            if rustix::fs::FileType::from_raw_mode(stat.st_mode)
                                != rustix::fs::FileType::RegularFile
                                || stat.st_nlink != 1 =>
                        {
                            return Err(expanded_error());
                        }
                        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error),
                        _ => {}
                    }
                }
            }
            let installed_before = previous
                .iter()
                .any(|(other, old)| other == id && old == version);
            let old_leaf = if installed_before && kind == "lib" {
                installed_wasm_name(&root, id, &metadata)?
            } else if installed_before && kind == "app" {
                installed_app_leaf(&root, id, &metadata)?
            } else {
                leaf.clone()
            };
            let candidate =
                if child_dir(package_stage, std::ffi::OsStr::new(&format!("new-{kind}"))).is_ok() {
                    Some(if kind == "lib" {
                        format!("{prefix}new-lib/{leaf}")
                    } else {
                        format!("{prefix}new-app")
                    })
                } else {
                    None
                };
            // Implicit-standard and explicit std/ names may address the same
            // physical file. The new candidate already journals that file.
            let physical = |identity: &str, path: &str| {
                if kind == "lib"
                    && super::is_standard_author(identity.split('/').next().unwrap_or(""))
                    && !package_name(identity).starts_with("std/")
                {
                    format!("std/{path}")
                } else {
                    path.to_owned()
                }
            };
            if candidate.is_none()
                && resources.iter().any(|resource| {
                    resource["kind"] == kind
                        && resource["candidate"].is_string()
                        && physical(
                            resource["id"].as_str().unwrap(),
                            resource["leaf"].as_str().unwrap(),
                        ) == physical(id, &old_leaf)
                })
            {
                continue;
            }
            resources.push(publication_resource(
                parent,
                id,
                version,
                kind,
                (&old_leaf, &leaf),
                candidate.as_deref(),
                false,
            )?);
        }
        active_names.insert(id);
    }
    let mut app_mounts = std::collections::BTreeMap::new();
    for id in active_names {
        let name = app_name(id);
        let active = registered_metadata(&tx, "app", id).ok();
        let has_app = active
            .as_ref()
            .is_some_and(|value| value["has_app"] == true);
        let entry = app_mounts.entry(name).or_insert((id, None));
        if has_app {
            *entry = (id, active);
        }
    }
    for (_, (id, active)) in app_mounts {
        if active.is_none() && app_mount_owned_by_other(&tx, id, None)? {
            continue;
        }
        let root = publication_root(parent, id, "app_link")?;
        let short = app_name(id).to_owned();
        // Preserve a legacy unversioned frontend in its version directory.
        if resource_stat(&root, &short).is_ok_and(|stat| {
            rustix::fs::FileType::from_raw_mode(stat.st_mode) == rustix::fs::FileType::Directory
        }) && !resources.iter().any(|resource| {
            resource["kind"] == "app" && resource["old"] == true && resource["old_leaf"] == short
        }) {
            for (old_id, old_version) in previous.iter().filter(|(old_id, _)| old_id == id) {
                let metadata = serde_json::json!({"version":old_version});
                let leaf = versioned_app_name(old_id, &metadata)?;
                if resource_stat(&root, &leaf).is_err() {
                    resources.push(publication_resource(
                        parent,
                        id,
                        old_version,
                        "app",
                        (&short, &leaf),
                        None,
                        true,
                    )?);
                    break;
                }
            }
        }
        let version = active
            .as_ref()
            .and_then(|value| value["version"].as_str())
            .unwrap_or(entries[0].1);
        if let Some(active) = &active {
            let leaf = versioned_app_name(id, active)?;
            if !resources
                .iter()
                .any(|resource| resource["kind"] == "app" && resource["leaf"] == leaf)
            {
                let old = installed_app_leaf(&root, id, active)?;
                if old != leaf {
                    resources.push(publication_resource(
                        parent,
                        id,
                        version,
                        "app",
                        (&old, &leaf),
                        None,
                        true,
                    )?);
                }
            }
        }
        // Older hosts published links at these mount paths. Remove them in the
        // same journal as the frontend directories so rollback can restore them.
        for path in std::iter::once(short.as_str())
            .chain((package_name(id) != short).then(|| package_name(id)))
        {
            if resource_stat(&root, path).is_ok_and(|stat| {
                rustix::fs::FileType::from_raw_mode(stat.st_mode) == rustix::fs::FileType::Symlink
            }) {
                resources.push(publication_resource(
                    parent,
                    id,
                    version,
                    "app_link",
                    (path, path),
                    None,
                    false,
                )?);
            }
        }
    }
    let journal = serde_json::json!({"format":2,"transaction":name,"resources":resources});
    let bytes =
        serde_json::to_vec(&journal).map_err(|error| Error::InvalidConfig(error.to_string()))?;
    if resources.len() > 500 || bytes.len() > 2 * 1024 * 1024 {
        return Err(Error::BadRequest(
            "package recovery journal exceeds 2 MiB".into(),
        ));
    }
    write_component(stage, ".transaction.json", &bytes)?;
    parent.sync_all()?;
    scopeguard::ScopeGuard::into_inner(cleanup);
    let result = (|| -> Result<()> {
        for (index, resource) in resources.iter().enumerate() {
            let root = publication_root(
                parent,
                resource["id"].as_str().unwrap(),
                resource["kind"].as_str().unwrap(),
            )?;
            let old_leaf = resource["old_leaf"].as_str().unwrap();
            let leaf = resource["leaf"].as_str().unwrap();
            let backup = format!("old-{index}");
            if resource["old"] == true {
                rename_resource(&root, old_leaf, stage, &backup)?;
            }
            if resource["reuse"] == true {
                rename_resource(stage, &backup, &root, leaf)?;
            } else if let Some(candidate) = resource["candidate"].as_str() {
                rename_resource(stage, candidate, &root, leaf)?;
            }
        }
        database_owner(profile)?;
        tx.execute(
            "INSERT INTO pkg_registry.package_transaction(id) VALUES(?1)",
            [name],
        )?;
        tx.commit()?;
        Ok(())
    })();
    if let Err(error) = result {
        let committed =
            crate::setup::open_config(profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .and_then(|database| {
                    database
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM pkg_registry.package_transaction WHERE id=?1)",
                    [name],
                    |row| row.get::<_, bool>(0),
                )
                .map_err(Error::from)
                });
        match committed {
            Ok(true) => {
                tracing::warn!(%error, "package commit confirmed through its transaction record")
            }
            Ok(false) => {
                if rollback_publication(parent, stage, &resources).is_ok() {
                    rustix::fs::unlinkat(stage, ".transaction.json", rustix::fs::AtFlags::empty())
                        .map_err(layout_error)?;
                    stage.sync_all()?;
                    cleanup_dir.remove_dir_all(name)?;
                    parent.sync_all()?;
                }
                return Err(error);
            }
            Err(_) => return Err(error),
        }
    }
    let cleanup = (|| -> Result<()> {
        rustix::fs::unlinkat(stage, ".transaction.json", rustix::fs::AtFlags::empty())
            .map_err(layout_error)?;
        stage.sync_all()?;
        cleanup_dir.remove_dir_all(name)?;
        parent.sync_all()?;
        let conn = crate::setup::open_config(profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        conn.execute(
            "DELETE FROM pkg_registry.package_transaction WHERE id=?1",
            [name],
        )?;
        if let Err(error) = prune_legacy_app_parents(parent, &resources) {
            tracing::warn!(%error, "empty legacy App directory cleanup deferred");
        }
        if let Err(error) = collect_release_cache(parent, &conn) {
            tracing::warn!(%error, "unreferenced package archive cleanup deferred");
        }
        Ok(())
    })();
    if let Err(error) = cleanup {
        tracing::warn!(%error, "committed package cleanup deferred to startup");
    }
    Ok(())
}

pub(crate) fn recover_version_publication(
    profile: &IdentityProfile,
    path: &std::path::Path,
    journal: &serde_json::Value,
) -> Result<()> {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(expanded_error)?;
    if journal["transaction"] != name {
        return Err(expanded_error());
    }
    let parent = profile_dir(profile)?;
    let stage = child_dir(&parent, std::ffi::OsStr::new(name))?;
    let resources = journal["resources"]
        .as_array()
        .filter(|resources| resources.len() <= 500)
        .ok_or_else(expanded_error)?;
    for resource in resources {
        let id = resource["id"].as_str().ok_or_else(expanded_error)?;
        let version = resource["version"].as_str().ok_or_else(expanded_error)?;
        valid_id(id)?;
        versioned_wasm_name(id, &serde_json::json!({"version":version}))?;
        let kind = resource["kind"].as_str().ok_or_else(expanded_error)?;
        let metadata = serde_json::json!({"version":version});
        let leaf = match kind {
            "lib" => versioned_wasm_name(id, &metadata)?,
            "app" => versioned_app_name(id, &metadata)?,
            "app_link" => app_name(id).to_owned(),
            "archive" => format!("{}-{version}.waz", package_name(id)),
            "pkg_archive" => release_cache_path(package_name(id), version)?,
            "volume" => super::renames::volume_move(id)
                .ok_or_else(expanded_error)?
                .1
                .to_owned(),
            _ => return Err(expanded_error()),
        };
        let previous_app_path = match kind {
            "app" => Some(format!("{}.{version}", package_name(id))),
            "app_link" => Some(package_name(id).to_owned()),
            _ => None,
        };
        if (resource["leaf"] != leaf && resource["leaf"].as_str() != previous_app_path.as_deref())
            || resource["old"].as_bool().is_none()
            || resource["reuse"].as_bool().is_none()
        {
            return Err(expanded_error());
        }
        let old = resource["old_leaf"].as_str().ok_or_else(expanded_error)?;
        if kind == "volume"
            && (!super::renames::volume_move(id).is_some_and(|(source, _)| old == source)
                || resource["reuse"] != true
                || resource["candidate"].is_string()
                || resource["old"] != true)
        {
            return Err(expanded_error());
        }
        if matches!(kind, "archive" | "pkg_archive") {
            let nested = format!(
                "{}.{version}/new-archive",
                package_name(id).replace('/', "%2F")
            );
            if old != leaf
                || resource["reuse"] != false
                || !matches!(resource["candidate"].as_str(), Some("new-archive"))
                    && resource["candidate"].as_str() != Some(nested.as_str())
            {
                return Err(expanded_error());
            }
        }
        let name = package_name(id);
        if old != leaf
            && old != name
            && old != name.replace('/', "%2F")
            && !legacy_wasm_names(id, version)
                .iter()
                .any(|path| path == old)
            && previous_app_path.as_deref() != Some(old)
            && !(kind == "app" && old == app_name(id))
            && !(kind == "volume"
                && super::renames::volume_move(id).is_some_and(|(source, _)| old == source)
                && resource["reuse"] == true
                && resource["candidate"].is_null())
        {
            return Err(expanded_error());
        }
        let root = publication_root(&parent, id, kind)?;
        resource_location(&root, old, false)?;
        if let Some(candidate) = resource["candidate"].as_str() {
            resource_location(&stage, candidate, false)?;
        }
    }
    let conn = crate::setup::open_config(profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    let committed: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pkg_registry.package_transaction WHERE id=?1)",
        [name],
        |row| row.get(0),
    )?;
    if !committed {
        rollback_publication(&parent, &stage, resources)?;
    }
    rustix::fs::unlinkat(&stage, ".transaction.json", rustix::fs::AtFlags::empty())
        .map_err(layout_error)?;
    stage.sync_all()?;
    cap_std::fs::Dir::from_std_file(parent.try_clone()?).remove_dir_all(name)?;
    parent.sync_all()?;
    conn.execute(
        "DELETE FROM pkg_registry.package_transaction WHERE id=?1",
        [name],
    )?;
    Ok(())
}

pub(crate) fn remove_package(profile: &IdentityProfile, id: &str) -> Result<()> {
    remove_package_version(profile, id, None)
}

pub(crate) fn remove_package_version(
    profile: &IdentityProfile,
    id: &str,
    version: Option<&str>,
) -> Result<()> {
    use rustix::fs::{Mode, mkdirat};
    valid_id(id)?;
    let parent = lock_package_writer(profile)?;
    let mut database = installation_database(profile, true)?;
    let tx = database.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let previous = registered_versions(&tx)?;
    let selected = previous
        .iter()
        .filter(|(candidate, candidate_version)| {
            candidate == id && version.is_none_or(|version| version == candidate_version)
        })
        .cloned()
        .collect::<Vec<_>>();
    if selected.is_empty() {
        return Ok(());
    }
    let short = package_name(id);
    if let Some(version) = version {
        let active: Option<String> = tx
            .query_row(
                "SELECT version FROM pkg_registry.package_active WHERE name=?1",
                [short],
                |row| row.get(0),
            )
            .optional()?;
        if active.as_deref() == Some(version) {
            return Err(super::signing::policy(
                "package_active",
                http::StatusCode::CONFLICT,
            ));
        }
    }
    let dependents = tx
        .prepare(
            r#"
SELECT package_name,package_version
FROM pkg_registry.package_dependency
WHERE dependency_name=?1 AND (?2 IS NULL OR version=?2)
ORDER BY package_name,package_version
"#,
        )?
        .query_map(rusqlite::params![short, version], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if dependents
        .iter()
        .any(|(name, _)| version.is_some() || name != short)
    {
        return Err(Error::PackageInUse(dependents));
    }
    let name = temporary_name()?;
    mkdirat(&parent, &name, Mode::from_raw_mode(0o700)).map_err(layout_error)?;
    let stage = child_dir(&parent, std::ffi::OsStr::new(&name))?;
    let mut stages = Vec::new();
    for (id, version) in &selected {
        let directory = format!("{}.{}", package_name(id).replace('/', "%2F"), version);
        mkdirat(&stage, directory.as_str(), Mode::from_raw_mode(0o700)).map_err(layout_error)?;
        stages.push((
            directory.clone(),
            child_dir(&stage, std::ffi::OsStr::new(&directory))?,
        ));
    }
    if version.is_none() {
        tx.execute(
            "DELETE FROM pkg_registry.package_active WHERE name=?1",
            [short],
        )?;
    }
    tx.execute(
        "DELETE FROM pkg_registry.package_root WHERE name=?1 AND (?2 IS NULL OR version=?2)",
        rusqlite::params![short, version],
    )?;
    tx.execute(
        "DELETE FROM pkg_registry.package_dependency WHERE package_name=?1 AND (?2 IS NULL OR package_version=?2)",
        rusqlite::params![short, version],
    )?;
    tx.execute(
        "DELETE FROM pkg_registry.package WHERE name=?1 AND (?2 IS NULL OR version=?2)",
        rusqlite::params![short, version],
    )?;
    refresh_capabilities(&tx, short)?;
    let entries = selected
        .iter()
        .zip(&stages)
        .map(|((id, version), (directory, stage))| {
            (id.as_str(), version.as_str(), directory.as_str(), stage)
        })
        .collect::<Vec<_>>();
    commit_packages_files(profile, &entries, &previous, &parent, &stage, &name, tx)
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
    let mut mounts = std::collections::HashSet::new();
    for (id, _) in &ids {
        if !mounts.insert(app_name(id)) {
            return Err(Error::InvalidConfig(format!(
                "duplicate App mount /app/{}",
                app_name(id)
            )));
        }
    }
    let device = root.metadata()?.dev();
    for (id, version) in ids {
        let directory =
            installed_app_directory(&root, &id, &serde_json::json!({"version":version}))?;
        if directory.metadata()?.dev() != device {
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

#[cfg(test)]
#[path = "../../tests/common/package_files.rs"]
mod test_support;
#[cfg(test)]
pub(crate) use test_support::{
    disk_bytes, install_app, install_lib, install_package, installed_libs, remove_app, remove_lib,
};
