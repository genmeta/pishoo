use super::*;
use crate::{
    package::{lib_metadata, read_package},
    sandbox::WasmRuntime,
};

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

// Isolated acceptance installs signed fixtures directly; production uses the
// complete dependency transaction in package::files::install_packages.
pub(crate) fn install_package(
    profile: &IdentityProfile,
    id: &str,
    bytes: &[u8],
    runtime: &WasmRuntime,
) -> Result<serde_json::Value> {
    use rustix::fs::{FlockOperation, Mode, flock, mkdirat};
    valid_id(id)?;
    if package_name(id) == "std" {
        return Err(Error::BadRequest("std is reserved".into()));
    }
    let parent = initialized_profile(profile)?;
    flock(&parent, FlockOperation::LockExclusive).map_err(std::io::Error::from)?;
    let package = read_package(bytes)?;
    let mut metadata = package_metadata(&package)?;
    let facts = crate::package::signing::facts(
        &package,
        metadata["publisher"].as_str().unwrap(),
        chrono::Utc::now().timestamp(),
    )?;
    metadata["author_owner_hash"] = facts["author_owner_hash"].clone();
    metadata["package_sha256"] =
        format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(bytes)).into();
    metadata["signer_leaf_fingerprint_sha256"] = facts["leaf_fingerprint_sha256"].clone();
    check_resource_identity(&metadata, id)?;
    if let Some(backend) = super::super::format::waz_backend(&package) {
        runtime.compile(backend)?;
    }
    let mut database = installation_database(profile, true)?;
    check_name(&database, id, "pkg")?;
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
        write_expanded(&dir, &package, id, kind)?;
    }
    let tx = database.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let previous = tx
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
    for kind in ["lib", "app"] {
        if metadata[format!("has_{kind}")] == true {
            check_routes(profile, &tx, id, kind, &package)?;
        }
    }
    // Fixtures deliberately replace same-version bytes to exercise runtime reloads.
    // Production installations enforce digest immutability before registration.
    tx.execute(
        "UPDATE pkg_registry.package SET package_sha256=NULL WHERE name=?1 AND version=?2",
        rusqlite::params![package_name(id), metadata["version"].as_str()],
    )?;
    save_package_identity(&tx, id, &metadata)?;
    save_package_relations(&tx, id, &metadata, &package)?;
    tx.execute(
        r#"
INSERT INTO pkg_registry.package_active(name,version) VALUES(?1,?2) ON CONFLICT(name) DO UPDATE
SET version=excluded.version
"#,
        rusqlite::params![package_name(id), metadata["version"].as_str()],
    )?;
    tx.execute(
        r#"
INSERT INTO pkg_registry.package_root(name,version) VALUES(?1,?2) ON CONFLICT(name) DO UPDATE
SET version=excluded.version
"#,
        rusqlite::params![package_name(id), metadata["version"].as_str()],
    )?;
    refresh_capabilities(&tx, package_name(id))?;
    cache_release(profile, bytes, metadata["package_sha256"].as_str().unwrap())?;
    scopeguard::ScopeGuard::into_inner(cleanup);
    commit_package_files(profile, id, &previous, &parent, &stage, &name, tx)?;
    Ok(lib_metadata(&package, Some(id)))
}

pub(crate) fn install_lib(
    profile: &IdentityProfile,
    id: &str,
    bytes: &[u8],
    runtime: &WasmRuntime,
) -> Result<serde_json::Value> {
    install_package(profile, id, bytes, runtime)
}

pub(crate) fn install_app(
    profile: &IdentityProfile,
    id: &str,
    bytes: &[u8],
) -> Result<serde_json::Value> {
    install_package(profile, id, bytes, &WasmRuntime::new()?)
}

pub(crate) fn remove_lib(profile: &IdentityProfile, id: &str) -> Result<()> {
    remove_package(profile, id)
}

pub(crate) fn remove_app(profile: &IdentityProfile, id: &str) -> Result<()> {
    remove_package(profile, id)
}

// Single-package fixtures use the same publication and recovery operation.
fn commit_package_files(
    profile: &IdentityProfile,
    id: &str,
    previous: &[(String, String)],
    parent: &std::fs::File,
    stage: &std::fs::File,
    name: &str,
    tx: rusqlite::Transaction<'_>,
) -> Result<()> {
    let version = registered_metadata(&tx, "pkg", id)
        .ok()
        .and_then(|metadata| metadata["version"].as_str().map(str::to_owned))
        .or_else(|| {
            previous
                .iter()
                .find(|(other, _)| other == id)
                .map(|(_, version)| version.clone())
        })
        .ok_or_else(expanded_error)?;
    commit_packages_files(
        profile,
        &[(id, &version, "", stage)],
        previous,
        parent,
        stage,
        name,
        tx,
    )
}
