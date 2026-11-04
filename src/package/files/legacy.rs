use super::*;

/// The host release, rather than an untrusted replacement package, grants these
/// exact historical transfers. Never load an override from the identity home.
pub(crate) fn migrate_legacy_packages(profile: &IdentityProfile) -> Result<()> {
    let groups: serde_json::Value = serde_json::from_str(include_str!("legacy/releases.json"))
        .map_err(|e| {
            Error::InvalidConfig(format!("invalid compiled migration authorization: {e}"))
        })?;
    migrate_legacy_groups(profile, &groups)
}

fn migrate_legacy_groups(profile: &IdentityProfile, groups: &serde_json::Value) -> Result<()> {
    let groups = groups
        .as_array()
        .filter(|groups| groups.len() <= 100)
        .ok_or_else(|| Error::InvalidConfig("invalid migration groups".into()))?;
    if groups.is_empty() {
        return Ok(());
    }
    let parent = profile_dir(profile)?;
    rustix::fs::flock(&parent, rustix::fs::FlockOperation::LockExclusive)
        .map_err(std::io::Error::from)?;
    let mut db = installation_database(profile, true)?;
    for group in groups {
        let result = (|| -> Result<()> {
            let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let entries = group["packages"]
                .as_array()
                .filter(|entries| !entries.is_empty() && entries.len() <= 100)
                .ok_or_else(|| Error::InvalidConfig("invalid migration package list".into()))?;
            let mut names = std::collections::BTreeSet::new();
            for entry in entries {
                names.insert(migration_text(entry, "name")?);
            }
            let mut changes = Vec::new();
            for name in names {
                let current = tx
                    .query_row(include_str!("legacy/sql/registration.sql"), [name], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, Option<String>>(2)?,
                            r.get::<_, Option<String>>(3)?,
                            r.get::<_, Option<String>>(4)?,
                            r.get::<_, bool>(5)?,
                            r.get::<_, bool>(6)?,
                        ))
                    })
                    .optional()?;
                let Some((author, version, owner, digest, fingerprint, has_lib, has_app)) = current
                else {
                    continue;
                };
                let candidates = entries
                    .iter()
                    .filter(|entry| entry["name"] == name)
                    .collect::<Vec<_>>();
                // Already transferred installations retain their own real version,
                // digest and signature facts, including after an application update.
                if candidates.iter().any(|entry| {
                    entry["target_author"] == author
                        && owner.as_deref() == entry["target_owner_hash"].as_str()
                }) {
                    continue;
                }
                let matches = candidates
                    .iter()
                    .filter(|entry| entry["author"] == author && entry["version"] == version)
                    .collect::<Vec<_>>();
                if matches.len() != 1 || fingerprint.is_some() {
                    return Err(Error::InvalidConfig(format!(
                        "no unique legacy authorization for {author}/{name}@{version}"
                    )));
                }
                let entry = matches[0];
                let old_owner = migration_text(entry, "author_owner_hash")?;
                let target_owner = migration_text(entry, "target_owner_hash")?;
                let target = migration_text(entry, "target_author")?;
                let original_digest = migration_text(entry, "package_sha256")?;
                for hash in [old_owner, target_owner, original_digest] {
                    if hash.len() != 64
                        || !hash
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    {
                        return Err(Error::InvalidConfig("invalid migration hash".into()));
                    }
                }
                valid_id(&format!("{target}/{name}"))?;
                if crate::package::is_standard_author(&author)
                    != crate::package::is_standard_author(target)
                    || owner.as_deref().is_some_and(|owner| owner != old_owner)
                    || digest
                        .as_deref()
                        .is_some_and(|digest| digest != original_digest)
                    || migration_text(entry, "evidence")?.trim().is_empty()
                {
                    return Err(Error::InvalidConfig(format!(
                        "legacy ownership or placement mismatch for {name}"
                    )));
                }
                verify_legacy_files(&parent, entry, has_lib, has_app)?;
                changes.push((name, target, target_owner));
            }
            for (name, target, owner) in changes {
                tx.execute(
                    include_str!("legacy/sql/transfer_author.sql"),
                    rusqlite::params![name, target, owner],
                )?;
            }
            tx.commit()?;
            Ok(())
        })();
        if let Err(error) = result {
            tracing::warn!(identity = profile.name(), group = ?group["group"], %error, "Legacy package transfer skipped; existing installation retained");
        }
    }
    Ok(())
}

fn migration_text<'a>(entry: &'a serde_json::Value, key: &str) -> Result<&'a str> {
    entry[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::InvalidConfig(format!("missing migration {key}")))
}

fn verify_legacy_files(
    parent: &std::fs::File,
    entry: &serde_json::Value,
    has_lib: bool,
    has_app: bool,
) -> Result<()> {
    use std::io::Read;

    use sha2::{Digest, Sha256};
    let name = migration_text(entry, "name")?;
    let author = migration_text(entry, "author")?;
    valid_id(&format!("{author}/{name}"))?;
    let expected = entry["files"]
        .as_object()
        .filter(|files| !files.is_empty() && files.len() <= 4096)
        .ok_or_else(|| Error::InvalidConfig("invalid legacy runtime file set".into()))?;
    let lib_path = format!(
        "lib/{}{name}.wasm",
        if crate::package::is_standard_author(author) {
            "std/"
        } else {
            ""
        }
    );
    let app_path = format!("app/{name}");
    if has_lib != expected.contains_key(&lib_path)
        || has_app
            != expected
                .keys()
                .any(|path| path.starts_with(&format!("{app_path}/")))
        || expected
            .keys()
            .any(|path| path != &lib_path && !path.starts_with(&format!("{app_path}/")))
    {
        return Err(Error::InvalidConfig(
            "legacy resource composition mismatch".into(),
        ));
    }
    let mut actual = std::collections::BTreeSet::new();
    if has_lib {
        actual.insert(lib_path.clone());
    }
    if has_app {
        let mut pending = vec![app_path.clone()];
        while let Some(path) = pending.pop() {
            let mut dir = parent.try_clone()?;
            for part in path.split('/') {
                dir = child_dir(&dir, std::ffi::OsStr::new(part))?;
            }
            if dir.metadata()?.dev() != parent.metadata()?.dev() {
                return Err(expanded_error());
            }
            for child in cap_std::fs::Dir::from_std_file(dir).entries()? {
                let child = child?;
                let file = child.file_name();
                let leaf = file.to_str().ok_or_else(expanded_error)?;
                let path = format!("{path}/{leaf}");
                if child.file_type()?.is_dir() {
                    if !expected
                        .keys()
                        .any(|key| key.starts_with(&format!("{path}/")))
                    {
                        return Err(expanded_error());
                    }
                    pending.push(path);
                } else if child.file_type()?.is_file() {
                    actual.insert(path);
                } else {
                    return Err(expanded_error());
                }
                if actual.len() + pending.len() > 8192 {
                    return Err(expanded_error());
                }
            }
        }
    }
    if actual != expected.keys().cloned().collect() {
        return Err(expanded_error());
    }
    let mut total = 0u64;
    for path in actual {
        let record = &expected[&path];
        let size = record["size"].as_u64().ok_or_else(expanded_error)?;
        total = total
            .checked_add(size)
            .filter(|total| *total <= 64 * 1024 * 1024)
            .ok_or_else(expanded_error)?;
        let mut file = expanded_file(parent, &path)?;
        let before = file.metadata()?;
        if before.nlink() != 1 || before.dev() != parent.metadata()?.dev() || before.len() != size {
            return Err(expanded_error());
        }
        let mut bytes = Vec::new();
        file.by_ref().take(size + 1).read_to_end(&mut bytes)?;
        let after = file.metadata()?;
        let reopened = expanded_file(parent, &path)?.metadata()?;
        if bytes.len() as u64 != size
            || format!("{:x}", Sha256::digest(&bytes)) != migration_text(record, "sha256")?
            || before.dev() != reopened.dev()
            || before.ino() != reopened.ino()
            || before.mtime() != after.mtime()
            || before.mtime_nsec() != after.mtime_nsec()
            || before.ctime() != after.ctime()
            || before.ctime_nsec() != after.ctime_nsec()
        {
            return Err(expanded_error());
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "../../../tests/unit/package/files/legacy.rs"]
mod tests;
