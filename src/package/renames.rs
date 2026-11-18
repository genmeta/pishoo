// Temporary compatibility for the Genmeta package-name transition. Delete this
// module and its call sites after old installations have been upgraded.
pub(super) fn canonical_name<'a>(author: &str, name: &'a str) -> &'a str {
    if author != "dev.genmeta.tech.dhttp.net" {
        return name;
    }
    match name {
        "profile" => "std/profile",
        "message" | "comm/message" => "std/comm/message",
        "comm/group" => "std/comm/group",
        "calendar" => "std/calendar",
        "medical" => "std/medical",
        "command" => "std/command",
        "python" => "std/python",
        _ => name,
    }
}

use std::collections::BTreeSet;

use super::*;

// Retire old identities and dependent historical releases together. An active
// dependent must have a replacement in this installation; signed dependency
// declarations are never rewritten to point at a different package.
pub(super) fn retire(
    tx: &rusqlite::Transaction<'_>,
    packages: &[(String, Vec<u8>, serde_json::Value)],
) -> Result<(Vec<(String, String)>, BTreeSet<String>, BTreeSet<String>)> {
    let mut removed = BTreeSet::new();
    let mut roots = BTreeSet::new();
    let mut active = BTreeSet::new();
    let mut identities = std::collections::BTreeMap::new();
    for (id, _, _) in packages {
        let (author, name) = id.split_once('/').ok_or(Error::RouteNotFound)?;
        let key = format!("{author}/{}", canonical_name(author, name));
        if identities.insert(key, id).is_some_and(|old| old != id) {
            return Err(Error::BadRequest(
                "cannot install predecessor and current package identities together".into(),
            ));
        }
    }
    for (id, _, proof) in packages {
        let (author, name) = id.split_once('/').ok_or(Error::RouteNotFound)?;
        if canonical_name(author, name) != name {
            continue;
        }
        let rows = tx
            .prepare(
                "SELECT name,version,author_owner_hash FROM pkg_registry.package WHERE author=?1",
            )?
            .query_map([author], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (old, version, owner) in rows {
            if old == name || canonical_name(author, &old) != name {
                continue;
            }
            if owner.as_deref() != proof["author_owner_hash"].as_str() || owner.is_none() {
                return Err(super::signing::policy(
                    "publisher_ownership_conflict",
                    http::StatusCode::CONFLICT,
                ));
            }
            if tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM pkg_registry.package_root WHERE name=?1)",
                [&old],
                |r| r.get::<_, bool>(0),
            )? {
                roots.insert(id.clone());
            }
            if tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM pkg_registry.package_active WHERE name=?1)",
                [&old],
                |r| r.get::<_, bool>(0),
            )? {
                active.insert(id.clone());
            }
            removed.insert((format!("{author}/{old}"), version));
        }
    }
    if removed.is_empty() {
        return Ok((Vec::new(), roots, active));
    }
    loop {
        let mut added = false;
        let edges = tx.prepare("SELECT coalesce(p.author||'/'||p.name,p.name),p.name,p.version,coalesce(t.author||'/'||d.dependency_name,d.dependency_name),d.version FROM pkg_registry.package_dependency d JOIN pkg_registry.package p ON p.name=d.package_name AND p.version=d.package_version JOIN pkg_registry.package t ON t.name=d.dependency_name AND t.version=d.version")?
            .query_map([], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, name, version, target_id, target_version) in edges {
            if !removed.contains(&(target_id, target_version))
                || removed.contains(&(id.clone(), version.clone()))
            {
                continue;
            }
            let was_active: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM pkg_registry.package_active WHERE name=?1 AND version=?2)", rusqlite::params![name,version], |r| r.get(0))?;
            let was_root: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM pkg_registry.package_root WHERE name=?1 AND version=?2)", rusqlite::params![name,version], |r| r.get(0))?;
            if (was_active || was_root) && !packages.iter().any(|(new, _, _)| new == &id) {
                return Err(Error::PackageInUse(vec![(name, version)]));
            }
            if was_active {
                active.insert(id.clone());
            }
            if was_root {
                roots.insert(id.clone());
            }
            added |= removed.insert((id, version));
        }
        if !added {
            break;
        }
    }
    for (id, version) in &removed {
        let name = id.split_once('/').map_or(id.as_str(), |(_, name)| name);
        for table in ["package_dependency", "package_root", "package_active"] {
            let (n, v) = if table == "package_dependency" {
                ("package_name", "package_version")
            } else {
                ("name", "version")
            };
            tx.execute(
                &format!("DELETE FROM pkg_registry.{table} WHERE {n}=?1 AND {v}=?2"),
                rusqlite::params![name, version],
            )?;
        }
    }
    for (id, version) in &removed {
        tx.execute(
            "DELETE FROM pkg_registry.package WHERE name=?1 AND version=?2",
            rusqlite::params![
                id.split_once('/').map_or(id.as_str(), |(_, name)| name),
                version
            ],
        )?;
    }
    Ok((removed.into_iter().collect(), roots, active))
}

// Compile against the existing volume handle. Its directory is moved only by
// the normal publication journal, so failed compilation leaves data in place.
pub(super) fn data_path(profile: &IdentityProfile, id: &str) -> Result<std::path::PathBuf> {
    let (author, name) = id.split_once('/').ok_or(Error::RouteNotFound)?;
    let canonical = profile.join(".volume").join(name);
    if author == "dev.genmeta.tech.dhttp.net" && name == "std/comm/message" {
        let old = profile.join(".volume/std/message");
        if old.try_exists()? {
            if canonical.try_exists()? {
                return Err(Error::InvalidConfig(
                    "both old and current Message data directories exist".into(),
                ));
            }
            return Ok(old);
        }
    }
    Ok(
        if super::is_standard_author(author) && !name.starts_with("std/") {
            profile.join(".volume/std").join(name)
        } else {
            canonical
        },
    )
}

pub(super) fn volume_move(id: &str) -> Option<(&'static str, &'static str)> {
    (id == "dev.genmeta.tech.dhttp.net/std/comm/message")
        .then_some(("std/message", "std/comm/message"))
}
