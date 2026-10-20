//! Package files: directory traversal, queries, atomic installation and removal.
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
    if !rustix::process::geteuid().is_root() {
        return Ok(());
    }
    let identity = profile_dir(profile)?;
    let db = child_dir(&identity, std::ffi::OsStr::new("db"))?;
    if db.metadata()?.dev() != identity.metadata()?.dev() {
        return Ok(());
    }
    inherit_owner(&db, &identity)?;
    for name in [
        "config.db",
        "config.db-wal",
        "config.db-shm",
        "config.db-journal",
    ] {
        match expanded_file(&db, name) {
            Ok(file) => {
                let metadata = file.metadata()?;
                if metadata.dev() != db.metadata()?.dev() || metadata.nlink() != 1 {
                    return Err(Error::InvalidConfig(
                        "database must be a local single-link regular file".into(),
                    ));
                }
                inherit_owner(&file, &db)?;
            }
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
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
    child_dir(&profile_dir(profile)?, std::ffi::OsStr::new("libs"))
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
    database_owner(profile)?;
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
        let legacy: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('libs') WHERE name='id') OR EXISTS(SELECT 1 FROM pragma_table_info('apps') WHERE name='id')", [], |row| row.get(0))?;
        if legacy {
            migrate_registration(&mut conn)?;
        }
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute_batch(include_str!("packages.sql"))?;
        for table in ["libs", "apps"] {
            let columns = tx
                .prepare(&format!("PRAGMA table_info({table})"))?
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            for name in ["title", "description", "license", "repository"] {
                if !columns.iter().any(|column| column == name) {
                    tx.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {name} TEXT"))?;
                }
            }
            for obsolete in ["package_sha256", "author_owner_hash"] {
                if columns.iter().any(|column| column == obsolete) {
                    tx.execute_batch(&format!("ALTER TABLE {table} DROP COLUMN {obsolete}"))?;
                }
            }
        }
        tx.commit()?;
    }
    database_owner(profile)?;
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
    // Rebuilding SQLite foreign-key targets requires disabling enforcement on
    // this connection before BEGIN. The final audit runs before atomic commit.
    conn.pragma_update(None, "foreign_keys", false)?;
    let result = (|| -> Result<()> {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for table in ["libs", "apps"] {
            let proof_column: bool = tx.query_row(&format!("SELECT EXISTS(SELECT 1 FROM pragma_table_info('{table}') WHERE name='package_proof')"), [], |row| row.get(0))?;
            if proof_column {
                let saved: bool = tx.query_row(
                    &format!(
                        "SELECT EXISTS(SELECT 1 FROM {table} WHERE package_proof IS NOT NULL)"
                    ),
                    [],
                    |row| row.get(0),
                )?;
                if saved {
                    return Err(Error::InvalidConfig("unpublished database proof layout requires explicit recovery before schema migration; data was preserved".into()));
                }
            }
        }
        registered_packages(&tx, "lib", None)?;
        registered_packages(&tx, "app", None)?;
        let tables = [
            "libs",
            "apps",
            "app_dependencies",
            "lib_dependencies",
            "lib_capabilities",
            "lib_capability_operations",
        ];
        let existing = tx
            .prepare("SELECT name FROM sqlite_master WHERE type='table'")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for table in tables {
            if existing
                .iter()
                .any(|name| name == &format!("pishoo_legacy_{table}"))
            {
                return Err(Error::InvalidConfig(
                    "conflicting package migration table".into(),
                ));
            }
            if existing.iter().any(|name| name == table) {
                tx.execute_batch(&format!(
                    "ALTER TABLE {table} RENAME TO pishoo_legacy_{table}"
                ))?;
            }
        }
        tx.execute_batch(include_str!("packages.sql"))?;
        for table in ["libs", "apps"] {
            if !existing.iter().any(|name| name == table) {
                continue;
            }
            let old = format!("pishoo_legacy_{table}");
            let columns = tx
                .prepare(&format!("PRAGMA table_info({old})"))?
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let author = if columns.iter().any(|name| name == "author") {
                "author"
            } else {
                "CASE WHEN instr(id,'/')=0 THEN NULL ELSE substr(id,1,instr(id,'/')-1) END"
            };
            let fields = ["title", "description", "license", "repository"]
                .iter()
                .map(|name| {
                    if columns.iter().any(|column| column == name) {
                        *name
                    } else {
                        "NULL"
                    }
                })
                .collect::<Vec<_>>()
                .join(",");
            tx.execute_batch(&format!("INSERT INTO {table}(name,author,version,title,description,license,repository) SELECT name,{author},version,{fields} FROM {old}"))?;
        }
        for (table, sql) in [
            (
                "app_dependencies",
                "INSERT INTO app_dependencies SELECT a.name,l.name,d.version FROM pishoo_legacy_app_dependencies d JOIN pishoo_legacy_apps a ON a.id=d.app_id JOIN pishoo_legacy_libs l ON l.id=d.lib_id",
            ),
            (
                "lib_dependencies",
                "INSERT INTO lib_dependencies SELECT l.name,t.name,d.version FROM pishoo_legacy_lib_dependencies d JOIN pishoo_legacy_libs l ON l.id=d.lib_id JOIN pishoo_legacy_libs t ON t.id=d.dependency_id",
            ),
            (
                "lib_capabilities",
                "INSERT INTO lib_capabilities SELECT l.name,c.capability_id,c.title FROM pishoo_legacy_lib_capabilities c JOIN pishoo_legacy_libs l ON l.id=c.lib_id",
            ),
            (
                "lib_capability_operations",
                "INSERT INTO lib_capability_operations SELECT l.name,c.capability_id,c.method,c.path FROM pishoo_legacy_lib_capability_operations c JOIN pishoo_legacy_libs l ON l.id=c.lib_id",
            ),
        ] {
            if !existing.iter().any(|name| name == table) {
                continue;
            }
            let old = format!("pishoo_legacy_{table}");
            let modern: bool = tx.query_row(
                &format!(
                    "SELECT EXISTS(SELECT 1 FROM pragma_table_info('{old}') WHERE name='lib_name')"
                ),
                [],
                |row| row.get(0),
            )?;
            if modern {
                tx.execute_batch(&format!("INSERT INTO {table} SELECT * FROM {old}"))?;
            } else {
                tx.execute_batch(sql)?;
            }
            let copied: bool = tx.query_row(
                &format!("SELECT (SELECT count(*) FROM {table})=(SELECT count(*) FROM {old})"),
                [],
                |row| row.get(0),
            )?;
            if !copied {
                return Err(Error::InvalidConfig(
                    "broken legacy package references".into(),
                ));
            }
        }
        for table in tables.into_iter().rev() {
            if existing.iter().any(|name| name == table) {
                tx.execute_batch(&format!("DROP TABLE pishoo_legacy_{table}"))?;
            }
        }
        let broken: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_foreign_key_check)",
            [],
            |row| row.get(0),
        )?;
        if broken {
            return Err(Error::InvalidConfig(
                "broken migrated package references".into(),
            ));
        }
        tx.commit()?;
        Ok(())
    })();
    conn.pragma_update(None, "foreign_keys", true)?;
    result
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
    let table = if kind == "app" { "apps" } else { "libs" };
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
                    let path = format!("/{}{suffix}", id.rsplit('/').next().unwrap());
                    routes.insert(path, vec!["GET".into(), "HEAD".into()]);
                }
            }
        }
        // The canonical root's redirect is also an actual static operation.
        routes.insert(
            format!("/{}", id.rsplit('/').next().unwrap()),
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
    if kind == "app" {
        let mount = format!("/{}", id.rsplit('/').next().unwrap());
        let locations = conn
            .prepare("SELECT location FROM proxy_locations")?
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
                    format!("App {mount} conflicts with configured proxy {location}"),
                )));
            }
        }
    }
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
                std::ffi::OsStr::new(&format!("{kind}s")),
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
                    if route == "/workspace" || route.starts_with("/workspace/") =>
                {
                    "/workspace"
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
    let name = id.rsplit('/').next().unwrap();
    let author = id.split_once('/').map(|(author, _)| author);
    if kind == "lib" {
        let incompatible: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM app_dependencies WHERE lib_name=?1 AND version<>?2) OR EXISTS(SELECT 1 FROM lib_dependencies WHERE dependency_name=?1 AND version<>?2)",rusqlite::params![name,api["version"].as_str().unwrap()],|row|row.get(0))?;
        if incompatible {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "lib version is required by installed dependencies",
            )));
        }
    }
    let table = if kind == "app" { "apps" } else { "libs" };
    conn.execute(&format!("INSERT INTO {table}(name,author,version) VALUES(?1,?2,?3) ON CONFLICT(name) DO UPDATE SET author=excluded.author,version=excluded.version"),
        rusqlite::params![name,author,api["version"].as_str().unwrap()])?;
    let (edges, source, target) = if kind == "app" {
        ("app_dependencies", "app_name", "lib_name")
    } else {
        ("lib_dependencies", "lib_name", "dependency_name")
    };
    conn.execute(&format!("DELETE FROM {edges} WHERE {source}=?1"), [name])?;
    for (dependency, version) in declared_dependencies(api) {
        let (dependency_author, dependency_name) = dependency.split_once('/').unwrap();
        let matches: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM libs WHERE author=?1 AND name=?2 AND version=?3)",
            rusqlite::params![dependency_author, dependency_name, version],
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
            rusqlite::params![name, dependency_name, version],
        )?;
    }

    if kind == "lib" {
        conn.execute("DELETE FROM lib_capabilities WHERE lib_name=?1", [name])?;
        for (capability, title, endpoints) in capability_rows(package)? {
            conn.execute(
                "INSERT INTO lib_capabilities(lib_name,capability_id,title) VALUES(?1,?2,?3)",
                rusqlite::params![name, capability, title],
            )?;
            for (method, path) in endpoints {
                conn.execute("INSERT INTO lib_capability_operations(lib_name,capability_id,method,path) VALUES(?1,?2,?3,?4)",rusqlite::params![name,capability,method,path])?;
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
            conn.prepare("SELECT dependency_name FROM lib_dependencies WHERE lib_name=?1")?
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

fn write_expanded(dir: &std::fs::File, bytes: &[u8], id: &str) -> Result<()> {
    let package = read_package(bytes)?;
    let metadata = package_metadata(&package)?;
    if metadata["kind"] == "lib" {
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
                let target = format!("public/{relative}");
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
    let table = if kind == "app" { "apps" } else { "libs" };
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
                child_dir(&app, std::ffi::OsStr::new("public"))
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
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use sha2::{Digest, Sha256};
    use std::io::Read;
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
    if let Some(repository) = metadata.get("repository") {
        wapm["repository"] = repository.clone();
    }
    let mut annotation = serde_json::json!({"kind":kind,"title":metadata["title"],"dependencies":metadata["dependencies"]});
    if kind == "app" {
        annotation["entry"] = "/".into();
    }
    let mount = if kind == "app" { "public" } else { "metadata" };
    let mut backend = Vec::new();
    if kind == "lib" {
        let parent = resource_parent(root, id, false)?;
        expanded_file(&parent, &format!("{}.wasm", id.rsplit('/').next().unwrap()))?
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
        let public = child_dir(&app, std::ffi::OsStr::new("public"))?;
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

fn registered_metadata(
    conn: &rusqlite::Connection,
    kind: &str,
    id: &str,
) -> Result<serde_json::Value> {
    let table = if kind == "app" { "apps" } else { "libs" };
    let (_, version) = registered_packages(conn, kind, Some(id))?.pop().unwrap();
    let columns = conn
        .prepare(&format!("PRAGMA table_info({table})"))?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let selected = ["title", "description", "license", "repository"]
        .iter()
        .map(|name| {
            if columns.iter().any(|column| column == name) {
                *name
            } else {
                "NULL"
            }
        })
        .collect::<Vec<_>>()
        .join(",");
    let identity = package_identity(conn, table)?;
    let fields = conn.query_row(
        &format!("SELECT {selected} FROM {table} WHERE ({identity})=?1"),
        [id],
        |row| {
            (0..4)
                .map(|index| row.get::<_, Option<String>>(index))
                .collect::<rusqlite::Result<Vec<_>>>()
        },
    )?;
    let name = id.rsplit('/').next().unwrap();
    let mut value = serde_json::json!({"id":id,"name":name,"version":version,"kind":kind,
        "title":fields[0].as_deref().unwrap_or(name),"description":fields[1],"dependencies":[]});
    if let Some((publisher, _)) = id.split_once('/') {
        value["publisher"] = publisher.into();
    }
    if kind == "app" {
        value["entry"] = "/".into();
    }
    for (index, key) in [(2, "license"), (3, "repository")] {
        if let Some(text) = &fields[index] {
            value[key] = text.clone().into();
        }
    }
    let (edges, source, target) = if kind == "app" {
        ("app_dependencies", "app_name", "lib_name")
    } else {
        ("lib_dependencies", "lib_name", "dependency_name")
    };
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [edges],
        |row| row.get(0),
    )?;
    if exists {
        let modern: bool = conn.query_row(
            &format!(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('{edges}') WHERE name='{source}')"
            ),
            [],
            |row| row.get(0),
        )?;
        let query = if modern {
            format!(
                "SELECT CASE WHEN l.author IS NULL THEN l.name ELSE l.author||'/'||l.name END,d.version FROM {edges} d JOIN libs l ON l.name=d.{target} WHERE d.{source}=?1 ORDER BY l.name"
            )
        } else if kind == "app" {
            "SELECT lib_id,version FROM app_dependencies WHERE app_id=?1 ORDER BY lib_id".into()
        } else {
            "SELECT dependency_id,version FROM lib_dependencies WHERE lib_id=?1 ORDER BY dependency_id".into()
        };
        let mut dependencies = Vec::new();
        for row in conn
            .prepare(&query)?
            .query_map([if modern { name } else { id }], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
        {
            let (dependency, version) = row?;
            let (publisher, name) = dependency
                .split_once('/')
                .ok_or_else(|| Error::InvalidConfig("invalid registered dependency ID".into()))?;
            dependencies
                .push(serde_json::json!({"publisher":publisher,"name":name,"version":version}));
        }
        value["dependencies"] = dependencies.into();
    }
    Ok(value)
}

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
    inherit_owner(&file, dir)?;
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
            "std is reserved for native services and standard Lib directories".into(),
        ));
    }
    if kind == "app" && !id.contains('/') {
        return Err(Error::BadRequest(
            "frontend app requires publisher/name ID".into(),
        ));
    }
    let parent = initialized_profile(profile)?;
    flock(&parent, FlockOperation::LockExclusive).map_err(std::io::Error::from)?;
    match mkdirat(&parent, format!("{kind}s"), Mode::from_raw_mode(0o700)) {
        Ok(()) => parent.sync_all()?,
        Err(rustix::io::Errno::EXIST) => {}
        Err(error) => return Err(layout_error(error)),
    }
    let root = child_dir(&parent, std::ffi::OsStr::new(&format!("{kind}s")))?;
    inherit_owner(&root, &parent)?;
    flock(&root, FlockOperation::LockShared).map_err(std::io::Error::from)?;
    disk_ids(&root)?;
    let mut database = installation_database(profile, true)?;
    check_name(&database, id, kind)?;
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
    let previous_version = match registered_packages(&database, kind, Some(id)) {
        Ok(mut rows) => Some(rows.pop().unwrap().1),
        Err(Error::RouteNotFound) => None,
        Err(error) => return Err(error),
    };
    let same_version = previous_version.as_deref() == metadata["version"].as_str();
    if kind == "lib" && previous_version.is_some() && !same_version {
        let name = id.rsplit('/').next().unwrap();
        let dependents: i64 = database.query_row("SELECT (SELECT count(*) FROM app_dependencies WHERE lib_name=?1)+(SELECT count(*) FROM lib_dependencies WHERE dependency_name=?1)", [name], |row| row.get(0))?;
        if dependents > 0 {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "lib version is required by installed App/Lib dependencies",
            )));
        }
    }
    for (dep_id, version) in declared_dependencies(&metadata) {
        if installed_libs(profile, Some(&dep_id))?["version"] != version {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("dependency {dep_id} requires {version}"),
            )));
        }
    }
    let name = temporary_name()?;
    mkdirat(&parent, &name, Mode::from_raw_mode(0o700)).map_err(layout_error)?;
    let cleanup_dir = cap_std::fs::Dir::from_std_file(parent.try_clone()?);
    let cleanup = scopeguard::guard((), |_| {
        let _ = cleanup_dir.remove_dir_all(&name);
    });
    let stage = child_dir(&parent, std::ffi::OsStr::new(&name))?;
    inherit_owner(&stage, &parent)?;
    write_expanded(&stage, bytes, id)?;
    flock(&root, FlockOperation::LockExclusive).map_err(std::io::Error::from)?;
    check_lib_root(profile, &parent, &root, kind)?;
    check_name(&database, id, kind)?;
    let target_parent = if kind == "lib" {
        resource_parent(&root, id, true)?
    } else {
        root.try_clone()?
    };
    if target_parent.metadata()?.dev() != parent.metadata()?.dev() {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "package directory must share the identity filesystem",
        )));
    }
    let short = id.rsplit('/').next().unwrap();
    let leaf = if kind == "lib" {
        format!("{short}.wasm")
    } else {
        short.to_owned()
    };
    let previous = if kind == "lib" {
        match expanded_file(&target_parent, &leaf) {
            Ok(file) => {
                if file.metadata()?.nlink() != 1 {
                    return Err(expanded_error());
                }
                true
            }
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error),
        }
    } else {
        match child_dir(&target_parent, std::ffi::OsStr::new(&leaf)) {
            Ok(dir) => {
                check_component_target(&dir, kind)?;
                true
            }
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error),
        }
    };
    let tx = database.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    check_routes(profile, &tx, id, kind, &package)?;
    check_existing_rules(profile, &tx, id, kind, &package, same_version)?;
    save_registration(&tx, id, kind, &metadata, &package)?;
    let table = if kind == "app" { "apps" } else { "libs" };
    tx.execute(
        &format!(
            "UPDATE {table} SET title=?2,description=?3,license=?4,repository=?5 WHERE name=?1"
        ),
        rusqlite::params![
            id.rsplit('/').next().unwrap(),
            metadata["title"].as_str(),
            metadata["description"].as_str(),
            metadata["license"].as_str(),
            metadata["repository"].as_str()
        ],
    )?;
    if previous {
        renameat(&target_parent, &leaf, &stage, ".previous").map_err(layout_error)?;
    }
    let restore = scopeguard::guard((), |_| {
        if previous {
            let _ = renameat(&stage, ".previous", &target_parent, &leaf);
            let _ = target_parent.sync_all();
        }
    });
    stage.sync_all()?;
    if kind == "lib" {
        renameat(&stage, &leaf, &target_parent, &leaf).map_err(layout_error)?;
    } else {
        renameat(&parent, &name, &target_parent, &leaf).map_err(layout_error)?;
    }
    let rollback = scopeguard::guard((), |_| {
        if kind == "lib" {
            let _ = renameat(&target_parent, &leaf, &stage, &leaf);
        } else {
            let _ = renameat(&target_parent, &leaf, &parent, &name);
        }
        let _ = target_parent.sync_all();
        let _ = parent.sync_all();
    });
    target_parent.sync_all()?;
    parent.sync_all()?;
    database_owner(profile)?;
    tx.commit()?;
    scopeguard::ScopeGuard::into_inner(rollback);
    scopeguard::ScopeGuard::into_inner(restore);
    if previous && kind == "app" {
        let published = cap_std::fs::Dir::from_std_file(stage.try_clone()?);
        let _ = published.remove_dir_all(".previous");
        let _ = stage.sync_all();
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
    let short_name = id.rsplit('/').next().unwrap();
    if kind == "lib" {
        let dependents = database
            .prepare(
                "SELECT 'app ' || CASE WHEN a.author IS NULL THEN a.name ELSE a.author||'/'||a.name END FROM app_dependencies d JOIN apps a ON a.name=d.app_name WHERE d.lib_name=?1 \
                 UNION SELECT 'lib ' || CASE WHEN l.author IS NULL THEN l.name ELSE l.author||'/'||l.name END FROM lib_dependencies d JOIN libs l ON l.name=d.lib_name WHERE d.dependency_name=?1 ORDER BY 1",
            )?
            .query_map([short_name], |row| row.get::<_, String>(0))?
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
    let table = if kind == "app" { "apps" } else { "libs" };
    let modern_schema: bool = database.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM pragma_table_info('{table}') WHERE name='author')"),
        [],
        |row| row.get(0),
    )?;
    let modern = if modern_schema {
        let target = if kind == "lib" {
            resource_parent(&root, id, false)
                .and_then(|parent| expanded_file(&parent, &format!("{short_name}.wasm")))
        } else {
            child_dir(&root, std::ffi::OsStr::new(short_name))
        };
        match target {
            Ok(_) => true,
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error),
        }
    } else {
        false
    };
    let target_parent = if modern {
        if kind == "lib" {
            resource_parent(&root, id, false)?
        } else {
            root.try_clone()?
        }
    } else {
        match resource_dir(&root, id) {
            Ok(_) => resource_parent(&root, id, false)?,
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                legacy_parent(&root, id, kind)?
            }
            Err(error) => return Err(error),
        }
    };
    let short = id.rsplit('/').next().unwrap();
    let leaf = if modern && kind == "lib" {
        format!("{short}.wasm")
    } else {
        short.to_owned()
    };
    if modern && kind == "lib" {
        expanded_file(&target_parent, &leaf)?;
    } else {
        check_component_target(
            &child_dir(&target_parent, std::ffi::OsStr::new(&leaf))?,
            kind,
        )?;
    }
    if parent.metadata()?.dev() != target_parent.metadata()?.dev() {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "package root must share the identity filesystem for atomic removal",
        )));
    }
    let name = temporary_name()?;
    let cleanup_dir = cap_std::fs::Dir::from_std_file(parent.try_clone()?);
    mkdirat(&parent, &name, Mode::from_raw_mode(0o700)).map_err(layout_error)?;
    let cleanup = scopeguard::guard((), |_| {
        let _ = cleanup_dir.remove_dir_all(&name);
    });
    let stage = child_dir(&parent, std::ffi::OsStr::new(&name))?;
    inherit_owner(&stage, &parent)?;
    let tx = database.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute(&format!("DELETE FROM {table} WHERE name=?1"), [short_name])?;
    renameat(&target_parent, &leaf, &stage, "artifact").map_err(layout_error)?;
    let rollback = scopeguard::guard((), |_| {
        let _ = renameat(&stage, "artifact", &target_parent, &leaf);
        let _ = target_parent.sync_all();
    });
    target_parent.sync_all()?;
    root.sync_all()?;
    stage.sync_all()?;
    parent.sync_all()?;
    database_owner(profile)?;
    tx.commit()?;
    scopeguard::ScopeGuard::into_inner(rollback);
    drop(cleanup);
    parent.sync_all()?;
    Ok(())
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
            installed_bytes(profile, &root, &id, "app")?.into(),
            webc::Version::V3,
        )
        .map_err(|error| Error::InvalidComponent(error.to_string()))?;
        package_metadata(&package)?;
        apps.insert(id, package);
    }
    Ok(apps)
}
