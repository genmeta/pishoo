//! Initialize identity directories and preserve their account ownership.
use dhttp_home::identity::IdentityProfile;

use crate::{Error, Result};

pub(super) fn initialize_directories(profile: &IdentityProfile) -> Result<()> {
    use rustix::fs::{Mode, OFlags, open};
    use std::os::unix::fs::MetadataExt;

    let root = std::fs::File::from(
        open(
            profile.path(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    );
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
    for directory in [
        "db",
        "file",
        "libs",
        "libs/std",
        "apps",
        "logs",
        "repo",
        "templates",
    ] {
        let path = profile.join(directory);
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        if !path.symlink_metadata()?.file_type().is_dir() {
            return Err(Error::InvalidConfig(format!(
                "{} must be a directory, not a symlink",
                path.display()
            )));
        }
    }
    if rustix::process::geteuid().is_root() {
        repair_identity_ownership(&root, &owner)?;
    }
    Ok(())
}

pub(super) fn repair_identity_ownership(
    directory: &std::fs::File,
    owner: &std::fs::Metadata,
) -> Result<()> {
    use rustix::fs::{Mode, OFlags, openat};
    use std::os::unix::fs::MetadataExt;

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
