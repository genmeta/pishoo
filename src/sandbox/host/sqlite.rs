//! Own SQLite unix-dotfile locks for the lifetime of their WASI Store.
use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, Read},
    os::unix::fs::MetadataExt as _,
    path::Path,
    sync::{Arc, Weak},
    time::Duration,
};

use cap_std::fs::Dir;
use preopens::Host as _;
use rustix::fs::{AtFlags, Mode, OFlags, fstat, mkdirat, openat, statat, unlinkat};
use types::{DescriptorFlags, ErrorCode, HostDescriptor, OpenFlags, PathFlags};
use wasmtime::component::{Linker, Resource};
use wasmtime_wasi::{
    filesystem::{Descriptor, WasiFilesystemView as _},
    p2::{
        FsResult,
        bindings::filesystem::{preopens, types},
    },
};

use super::StoreData;

type LockKey = (u64, u64, String);

#[derive(Default)]
pub(in crate::sandbox) struct SqliteLocks {
    handles: Vec<Weak<File>>,
    locks: BTreeMap<LockKey, LockDirectory>,
}

impl SqliteLocks {
    fn track(&mut self, handle: &Arc<File>) {
        if self.handles.len() == self.handles.capacity() {
            self.handles.retain(|handle| handle.strong_count() != 0);
        }
        self.handles.push(Arc::downgrade(handle));
    }
}

impl Drop for SqliteLocks {
    fn drop(&mut self) {
        if self.locks.is_empty() {
            return;
        }
        // WASI can have a blocking write still running after its future or
        // stream has been dropped. Those jobs hold the actual descriptor Arc.
        let mut handles = std::mem::take(&mut self.handles);
        handles.retain(|handle| handle.strong_count() != 0);
        let locks = std::mem::take(&mut self.locks);
        if handles.is_empty() {
            release(locks);
        } else {
            tokio::spawn(async move {
                while !handles.is_empty() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    handles.retain(|handle| handle.strong_count() != 0);
                }
                release(locks);
            });
        }
    }
}

struct LockDirectory {
    parent: File,
    name: String,
    inode: File,
}

impl LockDirectory {
    fn is_current(&self) -> io::Result<bool> {
        let stat = match statat(&self.parent, self.name.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(rustix::io::Errno::NOENT) => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        let original = fstat(&self.inode)?;
        Ok((stat.st_dev, stat.st_ino) == (original.st_dev, original.st_ino))
    }

    fn remove(&self) -> io::Result<()> {
        if self.is_current()? {
            unlinkat(&self.parent, self.name.as_str(), AtFlags::REMOVEDIR)?;
        }
        Ok(())
    }
}

fn release(locks: BTreeMap<LockKey, LockDirectory>) {
    for lock in locks.into_values() {
        if let Err(error) = lock.remove() {
            tracing::warn!(lock = %lock.name, %error, "cannot release WASM SQLite lock");
        }
    }
}

fn lock_path(
    state: &StoreData,
    descriptor: &Resource<Descriptor>,
    path: &str,
) -> Option<(File, LockKey)> {
    let name = Path::new(path).file_name()?.to_str()?;
    name.strip_suffix(".lock").filter(|name| !name.is_empty())?;
    let Descriptor::Dir(dir) = state.table.get(descriptor).ok()? else {
        return None;
    };
    if dir.perms.write_not_permitted() {
        return None;
    }
    // Use the granted directory capability to resolve nested database names.
    // Keep the leaf lexical so a trailing slash or parent traversal is not
    // silently normalized into a lock operation.
    if path.starts_with('/')
        || path.rsplit('/').next()? != name
        || path.split('/').any(|part| part == "..")
    {
        return None;
    }
    let dir = Dir::from_std_file(dir.dir.try_clone().ok()?);
    let parent = Path::new(path).parent()?;
    let parent = if parent.as_os_str().is_empty() {
        dir.into_std_file()
    } else {
        dir.open_dir(parent).ok()?.into_std_file()
    };
    let meta = parent.metadata().ok()?;
    Some((parent, (meta.dev(), meta.ino(), name.to_owned())))
}

fn sqlite_database(state: &SqliteLocks, parent: &File, name: &str) -> bool {
    let Ok(file) = openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) else {
        return false;
    };
    let mut file = File::from(file);
    let Ok(meta) = file.metadata() else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    if meta.len() == 0 {
        // A new database may be locked before the first header write. Its
        // connection must already have opened the file in this Store.
        return state
            .handles
            .iter()
            .filter_map(Weak::upgrade)
            .filter_map(|handle| handle.metadata().ok())
            .any(|opened| (opened.dev(), opened.ino()) == (meta.dev(), meta.ino()));
    }
    let mut header = [0; 16];
    file.read_exact(&mut header).is_ok() && &header == b"SQLite format 3\0"
}

fn acquire(state: &mut StoreData, parent: File, key: LockKey) -> io::Result<()> {
    let name = key.2.clone();
    mkdirat(&parent, name.as_str(), Mode::from_raw_mode(0o777))?;
    let inode = match openat(
        &parent,
        name.as_str(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(inode) => inode,
        Err(error) => {
            let _ = unlinkat(&parent, name.as_str(), AtFlags::REMOVEDIR);
            return Err(error.into());
        }
    };
    state.sqlite_locks.locks.insert(
        key,
        LockDirectory {
            parent,
            name,
            inode: inode.into(),
        },
    );
    Ok(())
}

fn guest_result<T>(outcome: FsResult<T>) -> wasmtime::Result<(Result<T, ErrorCode>,)> {
    Ok((match outcome {
        Ok(value) => Ok(value),
        Err(error) => Err(error.downcast()?),
    },))
}

pub(in crate::sandbox) fn add_to_linker(linker: &mut Linker<StoreData>) -> wasmtime::Result<()> {
    linker.allow_shadowing(true);
    linker
        .instance("wasi:filesystem/preopens@0.2.12")?
        .func_wrap("get-directories", |mut store, ()| {
            let state = store.data_mut();
            let preopens = state.filesystem().get_directories()?;
            for (descriptor, _) in &preopens {
                let Descriptor::Dir(dir) = state.table.get_mut(descriptor)? else {
                    continue;
                };
                // Each Store gets its own Arc. The Lib's preopen and other
                // invocations must not keep this Store's I/O alive.
                dir.dir = Arc::new(dir.dir.try_clone()?);
                state.sqlite_locks.track(&dir.dir);
            }
            Ok((preopens,))
        })?;
    let mut types = linker.instance("wasi:filesystem/types@0.2.12")?;
    types.func_wrap_async(
        "[method]descriptor.open-at",
        |mut store,
         (fd, path_flags, path, oflags, flags): (
            _,
            PathFlags,
            String,
            OpenFlags,
            DescriptorFlags,
        )| {
            Box::new(async move {
                let state = store.data_mut();
                let result = state
                    .filesystem()
                    .open_at(fd, path_flags, path, oflags, flags)
                    .await;
                if let Ok(resource) = &result {
                    let handle = match state.table.get(resource)? {
                        Descriptor::Dir(dir) => &dir.dir,
                        Descriptor::File(file) => &file.file,
                    };
                    state.sqlite_locks.track(handle);
                }
                guest_result(result)
            })
        },
    )?;
    types.func_wrap_async(
        "[method]descriptor.create-directory-at",
        |mut store, (fd, path): (_, String)| {
            Box::new(async move {
                let state = store.data_mut();
                if let Some((parent, key)) = lock_path(state, &fd, &path)
                    && sqlite_database(
                        &state.sqlite_locks,
                        &parent,
                        key.2.strip_suffix(".lock").unwrap(),
                    )
                {
                    return guest_result(acquire(state, parent, key).map_err(Into::into));
                }
                guest_result(state.filesystem().create_directory_at(fd, path).await)
            })
        },
    )?;
    types.func_wrap_async(
        "[method]descriptor.remove-directory-at",
        |mut store, (fd, path): (_, String)| {
            Box::new(async move {
                let state = store.data_mut();
                if let Some((_, key)) = lock_path(state, &fd, &path)
                    && let Some(lock) = state.sqlite_locks.locks.get(&key)
                {
                    let result = lock.remove();
                    if result.is_ok() {
                        state.sqlite_locks.locks.remove(&key);
                    }
                    return guest_result(result.map_err(Into::into));
                }
                guest_result(state.filesystem().remove_directory_at(fd, path).await)
            })
        },
    )?;
    types.func_wrap_async(
        "[method]descriptor.stat-at",
        |mut store, (fd, flags, path): (_, PathFlags, String)| {
            Box::new(async move {
                let state = store.data_mut();
                if let Some((_, key)) = lock_path(state, &fd, &path)
                    && let Some(lock) = state.sqlite_locks.locks.get(&key)
                    && lock.is_current().unwrap_or(false)
                {
                    // SQLite's unix-dotfile reserved-lock check is access(.lock)
                    // after acquiring SHARED. Its own directory does not indicate
                    // another connection, and hiding it here enables hot rollback.
                    return Ok((Err(ErrorCode::NoEntry),));
                }
                guest_result(state.filesystem().stat_at(fd, flags, path).await)
            })
        },
    )?;
    linker.allow_shadowing(false);
    Ok(())
}

#[cfg(test)]
#[path = "../../../tests/unit/sandbox/host/sqlite.rs"]
mod tests;
