//! Initialize identity directories and preserve their account ownership.
use dhttp_home::identity::IdentityProfile;

use crate::{Error, Result};

pub(super) fn initialize_directories(profile: &IdentityProfile) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    use rustix::fs::{Mode, OFlags, open};

    let root = std::fs::File::from(
        open(
            profile.path(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    );
    rustix::fs::flock(&root, rustix::fs::FlockOperation::LockExclusive)
        .map_err(std::io::Error::from)?;
    let mut owner = root.metadata()?;
    if rustix::process::geteuid().is_root() {
        // A personal identity belongs to the account owning its home, including
        // when an earlier root process created .dhttp or the identity itself.
        if let Some(personal) = profile
            .path()
            .parent()
            .filter(|path| path.file_name().is_some_and(|name| name == ".dhttp"))
        {
            let home = personal.parent().ok_or_else(|| {
                Error::InvalidConfig("personal identity has no account home".into())
            })?;
            let home = std::fs::File::from(
                open(
                    home,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(std::io::Error::from)?,
            );
            owner = home.metadata()?;
            let personal = std::fs::File::from(
                open(
                    personal,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(std::io::Error::from)?,
            );
            for directory in [&personal, &root] {
                let permissions = directory.metadata()?.permissions();
                rustix::fs::fchown(
                    directory,
                    Some(rustix::fs::Uid::from_raw(owner.uid())),
                    Some(rustix::fs::Gid::from_raw(owner.gid())),
                )
                .map_err(std::io::Error::from)?;
                directory.set_permissions(permissions)?;
            }
        }
    }
    for (old, new) in [("libs", "lib"), ("apps", "app"), ("logs", "log")] {
        let source = profile.join(old);
        let target = profile.join(new);
        if source.try_exists()? {
            if !source.symlink_metadata()?.file_type().is_dir() {
                return Err(Error::InvalidConfig(
                    "legacy runtime root must be a real directory".into(),
                ));
            }
            if target.try_exists()? {
                return Err(Error::InvalidConfig(format!(
                    "both {old} and {new} exist; reconcile them before migration"
                )));
            }
            std::fs::rename(source, target)?;
            root.sync_all()?;
        }
    }
    for directory in [
        ".volume",
        ".volume/sys",
        ".volume/sys/access",
        ".volume/sys/contact",
        ".volume/sys/setting",
        ".volume/sys/pkg",
        ".volume/std",
        "lib",
        "lib/std",
        "app",
        "log",
        "repo",
    ] {
        let path = profile.join(directory);
        match std::fs::DirBuilder::new().create(&path) {
            Ok(()) => {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        if !path.symlink_metadata()?.file_type().is_dir() {
            return Err(Error::InvalidConfig(
                "identity layout must contain real directories".into(),
            ));
        }
    }
    // SQLite backup includes committed WAL content and leaves the old database intact.
    for (old, new) in [
        ("db/access.db", ".volume/sys/access/database"),
        ("db/contact.db", ".volume/sys/contact/database"),
    ] {
        let source = profile.join(old);
        let target = profile.join(new);
        if !target.try_exists()? && source.try_exists()? {
            if !source.symlink_metadata()?.file_type().is_file() {
                return Err(Error::InvalidConfig(
                    "legacy database must be a regular file".into(),
                ));
            }
            let staging = tempfile::NamedTempFile::new_in(target.parent().unwrap())?;
            let db = rusqlite::Connection::open_with_flags(
                source,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )?;
            db.backup(rusqlite::DatabaseName::Main, staging.path(), None)?;
            staging.as_file().sync_all()?;
            staging
                .persist_noclobber(target)
                .map_err(|error| error.error)?;
        }
    }
    for entry in std::fs::read_dir(profile.join("app"))? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let app = entry.path();
        // Old signed/expanded archives retain their literal span paths until reinstall.
        if app.join(".webc-proof").try_exists()? || app.join("app.webc").try_exists()? {
            continue;
        }
        let public = app.join("public");
        if public.try_exists()? {
            if !public.symlink_metadata()?.file_type().is_dir() {
                return Err(Error::InvalidConfig(
                    "public must be a real directory".into(),
                ));
            }
            for file in std::fs::read_dir(&public)? {
                let file = file?;
                let target = app.join(file.file_name());
                if target.try_exists()? {
                    return Err(Error::InvalidConfig(
                        "conflicting App public migration".into(),
                    ));
                }
                std::fs::rename(file.path(), target)?;
            }
            std::fs::remove_dir(public)?;
            std::fs::File::open(app)?.sync_all()?;
        }
    }
    recover_package_files(profile)?;
    if rustix::process::geteuid().is_root() {
        repair_identity_ownership(&root, &owner)?;
    }
    Ok(())
}

pub(super) fn repair_identity_ownership(
    directory: &std::fs::File,
    owner: &std::fs::Metadata,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    use rustix::fs::{Mode, OFlags, openat};

    let device = directory.metadata()?.dev();
    let mut directories = vec![directory.try_clone()?];
    while let Some(directory) = directories.pop() {
        let directory = cap_std::fs::Dir::from_std_file(directory);
        for entry in directory.entries()? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if !kind.is_dir() && !kind.is_file() {
                continue;
            }
            let file = std::fs::File::from(
                openat(
                    &directory,
                    entry.file_name(),
                    OFlags::RDONLY
                        | OFlags::NOFOLLOW
                        | OFlags::NONBLOCK
                        | OFlags::CLOEXEC
                        | if kind.is_dir() {
                            OFlags::DIRECTORY
                        } else {
                            OFlags::empty()
                        },
                    Mode::empty(),
                )
                .map_err(std::io::Error::from)?,
            );
            let metadata = file.metadata()?;
            // A linked file may also belong to a path outside the identity. Do not
            // change that inode, or descend into a different mounted filesystem.
            if metadata.dev() != device || (!metadata.is_dir() && metadata.nlink() != 1) {
                continue;
            }
            if !metadata.is_file() && !metadata.is_dir() {
                continue;
            }
            if metadata.uid() != owner.uid() || metadata.gid() != owner.gid() {
                rustix::fs::fchown(
                    &file,
                    Some(rustix::fs::Uid::from_raw(owner.uid())),
                    Some(rustix::fs::Gid::from_raw(owner.gid())),
                )
                .map_err(std::io::Error::from)?;
                file.set_permissions(metadata.permissions())?;
            }
            if metadata.is_dir() {
                directories.push(file);
            }
        }
    }
    Ok(())
}

pub(super) fn inherit_identity_owner(
    file: &std::fs::File,
    profile: &IdentityProfile,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    if rustix::process::geteuid().is_root() {
        let current = file.metadata()?;
        let owner = profile.path().symlink_metadata()?;
        if !owner.is_dir() || (!current.is_dir() && (!current.is_file() || current.nlink() != 1)) {
            return Err(Error::InvalidConfig(
                "identity resource must be an owned directory or single-link regular file".into(),
            ));
        }
        if current.uid() != owner.uid() || current.gid() != owner.gid() {
            rustix::fs::fchown(
                file,
                Some(rustix::fs::Uid::from_raw(owner.uid())),
                Some(rustix::fs::Gid::from_raw(owner.gid())),
            )
            .map_err(std::io::Error::from)?;
            file.set_permissions(current.permissions())?;
        }
    }
    Ok(())
}

// Temporary operation files are removed after commit. They are not installation
// metadata: recovery compares complete SQLite facts, then finishes or rolls back.
fn recover_package_files(profile: &IdentityProfile) -> Result<()> {
    for entry in std::fs::read_dir(profile.path())? {
        let entry = entry?;
        if !entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with(".pishoo-"))
            || !entry.file_type()?.is_dir()
        {
            continue;
        }
        let stage = entry.path();
        let marker = stage.join(".transaction.json");
        if !marker.try_exists()? {
            continue;
        }
        let stat = marker.symlink_metadata()?;
        if !stat.file_type().is_file() || stat.len() > 2 * 1024 * 1024 {
            return Err(Error::InvalidConfig(
                "invalid package recovery journal".into(),
            ));
        }
        let journal: serde_json::Value = serde_json::from_slice(&std::fs::read(&marker)?)
            .map_err(|e| Error::InvalidConfig(e.to_string()))?;
        let id = journal["id"]
            .as_str()
            .ok_or_else(|| Error::InvalidConfig("missing recovery identity".into()))?;
        crate::package::valid_id(id)?;
        let short = id.rsplit('/').next().unwrap();
        let resources = journal["resources"]
            .as_array()
            .filter(|r| r.len() == 2)
            .ok_or_else(|| Error::InvalidConfig("invalid package recovery resources".into()))?;
        let db = crate::setup::open_config(profile, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let current: String = db.query_row(
            include_str!("../package/registration-facts.sql"),
            [short],
            |r| r.get(0),
        )?;
        let committed = journal["facts"].as_str() == Some(current.as_str());
        for (resource, kind) in resources.iter().zip(["lib", "app"]) {
            if resource["kind"] != kind
                || resource["old"].as_bool().is_none()
                || resource["new"].as_bool().is_none()
            {
                return Err(Error::InvalidConfig("invalid recovery resource".into()));
            }
            let leaf = if kind == "lib" {
                format!("{short}.wasm")
            } else {
                short.to_owned()
            };
            let root = profile.join(if kind == "lib" && id.starts_with("std.dhttp.net/") {
                "lib/std"
            } else {
                kind
            });
            if !root.symlink_metadata()?.file_type().is_dir() {
                return Err(Error::InvalidConfig(
                    "recovery target must be a real directory".into(),
                ));
            }
            let target = root.join(&leaf);
            let backup = stage.join(format!("old-{kind}"));
            let candidate = if kind == "lib" {
                stage.join("new-lib").join(&leaf)
            } else {
                stage.join("new-app")
            };
            let remove = |path: &std::path::Path| -> Result<()> {
                match path.symlink_metadata() {
                    Ok(stat) if kind == "lib" && stat.file_type().is_file() => {
                        std::fs::remove_file(path)?
                    }
                    Ok(stat) if kind == "app" && stat.file_type().is_dir() => {
                        std::fs::remove_dir_all(path)?
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                    _ => return Err(Error::InvalidConfig("invalid recovery file type".into())),
                }
                Ok(())
            };
            if !committed {
                if backup.try_exists()? {
                    remove(&target)?;
                    std::fs::rename(&backup, &target)?;
                } else if resource["old"] == false
                    && resource["new"] == true
                    && !candidate.try_exists()?
                {
                    remove(&target)?;
                }
            }
            std::fs::File::open(root)?.sync_all()?;
        }
        std::fs::remove_dir_all(stage)?;
        std::fs::File::open(profile.path())?.sync_all()?;
    }
    Ok(())
}
