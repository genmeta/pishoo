//! SQLite's file operations are relative to a host-owned directory capability.
//! Each connection owns its registration. No ambient path or guest-provided VFS
//! name is used; SQLite never receives a path to the host filesystem.
#![allow(unsafe_op_in_unsafe_fn)]
use super::{Failure, fail, sql, storage};
use cap_std::fs::Dir;
use rusqlite::{Connection, OpenFlags, ffi::*};
use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    os::unix::fs::FileExt,
    sync::Arc,
};

struct Vfs {
    raw: sqlite3_vfs,
    root: Arc<Dir>,
    name: CString,
}
#[repr(C)]
struct File {
    raw: sqlite3_file,
    file: Option<std::fs::File>,
    lock: c_int,
}

pub(super) fn with<T>(
    root: Arc<Dir>,
    action: impl FnOnce(&mut Connection) -> Result<T, Failure>,
) -> Result<T, Failure> {
    let mut vfs = Box::new(Vfs {
        raw: unsafe { std::mem::zeroed() },
        root,
        name: CString::new(format!("pishoo-file-{}", super::random_id()?)).unwrap(),
    });
    vfs.raw.iVersion = 1;
    vfs.raw.szOsFile = std::mem::size_of::<File>() as c_int;
    vfs.raw.mxPathname = 64;
    vfs.raw.zName = vfs.name.as_ptr();
    vfs.raw.pAppData = (&mut *vfs as *mut Vfs).cast();
    vfs.raw.xOpen = Some(open);
    vfs.raw.xDelete = Some(delete);
    vfs.raw.xAccess = Some(access);
    vfs.raw.xFullPathname = Some(full_path);
    vfs.raw.xRandomness = Some(random);
    vfs.raw.xSleep = Some(sleep);
    vfs.raw.xCurrentTime = Some(time);
    let rc = unsafe { sqlite3_vfs_register(&mut vfs.raw, 0) };
    if rc != SQLITE_OK {
        return Err(fail(503, "file index VFS registration failed"));
    }
    let registered = scopeguard::guard(vfs, |mut vfs| unsafe {
        sqlite3_vfs_unregister(&mut vfs.raw);
    });
    let mut db = Connection::open_with_flags_and_vfs(
        "database",
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        registered.name.to_str().unwrap(),
    )
    .map_err(storage)?;
    unsafe {
        // A guest can edit its own index. Bound record allocation and prohibit
        // extra databases independently of the host's fixed SQL statements.
        sqlite3_limit(db.handle(), SQLITE_LIMIT_LENGTH, 16 * 1024);
        sqlite3_limit(db.handle(), SQLITE_LIMIT_ATTACHED, 0);
    }
    db.busy_timeout(std::time::Duration::from_secs(2))
        .map_err(storage)?;
    db.execute_batch(sql::CONNECTION).map_err(storage)?;
    // SQL is compiled into the host, never supplied by the guest. In particular
    // ATTACH, load_extension, WAL, and arbitrary filenames have no entry point.
    action(&mut db)
}
unsafe fn root<'a>(vfs: *mut sqlite3_vfs) -> &'a Dir {
    &(*((*vfs).pAppData as *mut Vfs)).root
}
unsafe fn filename<'a>(name: *const c_char) -> Option<&'a str> {
    if name.is_null() {
        return None;
    }
    match CStr::from_ptr(name).to_str().ok()? {
        "database" => Some("database"),
        "database-journal" => Some("database-journal"),
        _ => None,
    }
}
unsafe extern "C" fn open(
    vfs: *mut sqlite3_vfs,
    name: *const c_char,
    out: *mut sqlite3_file,
    flags: c_int,
    result: *mut c_int,
) -> c_int {
    (*out).pMethods = std::ptr::null();
    let Some(name) = filename(name) else {
        return SQLITE_CANTOPEN;
    };
    if flags & (SQLITE_OPEN_DELETEONCLOSE | SQLITE_OPEN_WAL) != 0 {
        return SQLITE_CANTOPEN;
    }
    let mut options = rustix::fs::OFlags::RDWR
        | rustix::fs::OFlags::NOFOLLOW
        | rustix::fs::OFlags::CLOEXEC
        | rustix::fs::OFlags::NONBLOCK;
    if flags & SQLITE_OPEN_CREATE != 0 {
        options |= rustix::fs::OFlags::CREATE;
    }
    if flags & SQLITE_OPEN_EXCLUSIVE != 0 {
        options |= rustix::fs::OFlags::EXCL;
    }
    let Ok(fd) = rustix::fs::openat(
        root(vfs),
        name,
        options,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    ) else {
        return SQLITE_CANTOPEN;
    };
    let file = std::fs::File::from(fd);
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return SQLITE_CANTOPEN;
    }
    if flags & SQLITE_OPEN_CREATE != 0
        && root(vfs)
            .try_clone()
            .and_then(|d| d.into_std_file().sync_all())
            .is_err()
    {
        return SQLITE_IOERR_FSYNC;
    }
    (out as *mut File).write(File {
        raw: sqlite3_file { pMethods: &METHODS },
        file: Some(file),
        lock: SQLITE_LOCK_NONE,
    });
    if !result.is_null() {
        *result = SQLITE_OPEN_READWRITE;
    }
    SQLITE_OK
}
unsafe extern "C" fn delete(vfs: *mut sqlite3_vfs, name: *const c_char, sync: c_int) -> c_int {
    let Some(name) = filename(name) else {
        return SQLITE_IOERR_DELETE;
    };
    match root(vfs).remove_file(name) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return SQLITE_IOERR_DELETE_NOENT,
        Err(_) => return SQLITE_IOERR_DELETE,
    }
    if sync != 0
        && root(vfs)
            .try_clone()
            .and_then(|d| d.into_std_file().sync_all())
            .is_err()
    {
        return SQLITE_IOERR_DIR_FSYNC;
    }
    SQLITE_OK
}
unsafe extern "C" fn access(
    vfs: *mut sqlite3_vfs,
    name: *const c_char,
    _: c_int,
    out: *mut c_int,
) -> c_int {
    *out = 0;
    // SQLite may probe -wal even though this VFS does not support it.
    let Some(name) = filename(name) else {
        return SQLITE_OK;
    };
    match root(vfs).symlink_metadata(name) {
        Ok(m) if m.is_file() => {
            *out = 1;
            SQLITE_OK
        }
        Ok(_) => SQLITE_CANTOPEN,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => SQLITE_OK,
        Err(_) => SQLITE_IOERR_ACCESS,
    }
}
unsafe extern "C" fn full_path(
    _: *mut sqlite3_vfs,
    name: *const c_char,
    size: c_int,
    out: *mut c_char,
) -> c_int {
    let Some(name) = filename(name) else {
        return SQLITE_CANTOPEN;
    };
    if size <= name.len() as c_int {
        return SQLITE_CANTOPEN;
    }
    std::ptr::copy_nonoverlapping(name.as_ptr(), out.cast(), name.len());
    *out.add(name.len()) = 0;
    SQLITE_OK
}
unsafe extern "C" fn random(_: *mut sqlite3_vfs, len: c_int, out: *mut c_char) -> c_int {
    if len < 0 {
        return 0;
    }
    if getrandom::fill(std::slice::from_raw_parts_mut(out.cast(), len as usize)).is_ok() {
        len
    } else {
        0
    }
}
unsafe extern "C" fn sleep(_: *mut sqlite3_vfs, micros: c_int) -> c_int {
    if micros > 0 {
        std::thread::sleep(std::time::Duration::from_micros(micros as u64));
    }
    micros.max(0)
}
unsafe extern "C" fn time(_: *mut sqlite3_vfs, out: *mut f64) -> c_int {
    *out = chrono::Utc::now().timestamp_millis() as f64 / 86_400_000.0 + 2_440_587.5;
    SQLITE_OK
}
unsafe fn file<'a>(raw: *mut sqlite3_file) -> &'a mut File {
    &mut *raw.cast::<File>()
}
unsafe extern "C" fn close(raw: *mut sqlite3_file) -> c_int {
    file(raw).file.take();
    SQLITE_OK
}
unsafe extern "C" fn read(
    raw: *mut sqlite3_file,
    out: *mut c_void,
    len: c_int,
    offset: i64,
) -> c_int {
    if len < 0 || offset < 0 {
        return SQLITE_IOERR_READ;
    }
    let buf = std::slice::from_raw_parts_mut(out.cast(), len as usize);
    buf.fill(0);
    let mut done = 0;
    while done < buf.len() {
        match file(raw)
            .file
            .as_ref()
            .unwrap()
            .read_at(&mut buf[done..], offset as u64 + done as u64)
        {
            Ok(0) => return SQLITE_IOERR_SHORT_READ,
            Ok(n) => done += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return SQLITE_IOERR_READ,
        }
    }
    SQLITE_OK
}
unsafe extern "C" fn write(
    raw: *mut sqlite3_file,
    input: *const c_void,
    len: c_int,
    offset: i64,
) -> c_int {
    if len < 0 || offset < 0 {
        return SQLITE_IOERR_WRITE;
    }
    let buf = std::slice::from_raw_parts(input.cast(), len as usize);
    match file(raw)
        .file
        .as_ref()
        .unwrap()
        .write_all_at(buf, offset as u64)
    {
        Ok(()) => SQLITE_OK,
        Err(_) => SQLITE_IOERR_WRITE,
    }
}
unsafe extern "C" fn truncate(raw: *mut sqlite3_file, len: i64) -> c_int {
    if len < 0 {
        return SQLITE_IOERR_TRUNCATE;
    }
    match file(raw).file.as_ref().unwrap().set_len(len as u64) {
        Ok(()) => SQLITE_OK,
        Err(_) => SQLITE_IOERR_TRUNCATE,
    }
}
unsafe extern "C" fn sync(raw: *mut sqlite3_file, _: c_int) -> c_int {
    match file(raw).file.as_ref().unwrap().sync_all() {
        Ok(()) => SQLITE_OK,
        Err(_) => SQLITE_IOERR_FSYNC,
    }
}
unsafe extern "C" fn size(raw: *mut sqlite3_file, out: *mut i64) -> c_int {
    match file(raw).file.as_ref().unwrap().metadata() {
        Ok(m) => {
            *out = m.len() as i64;
            SQLITE_OK
        }
        Err(_) => SQLITE_IOERR_FSTAT,
    }
}
unsafe extern "C" fn lock(raw: *mut sqlite3_file, level: c_int) -> c_int {
    let file = file(raw);
    if file.lock == SQLITE_LOCK_NONE {
        // Conservatively serialize even read snapshots. One OS lock covers the
        // SQLite transaction; upgrades cannot lose a shared lock, and separate
        // connections in the same process remain mutually exclusive.
        if file.file.as_ref().unwrap().try_lock().is_err() {
            return SQLITE_BUSY;
        }
    }
    file.lock = level;
    SQLITE_OK
}
unsafe extern "C" fn unlock(raw: *mut sqlite3_file, level: c_int) -> c_int {
    let file = file(raw);
    if level == SQLITE_LOCK_NONE && file.file.as_ref().unwrap().unlock().is_err() {
        return SQLITE_IOERR_UNLOCK;
    }
    file.lock = level;
    SQLITE_OK
}
unsafe extern "C" fn reserved(raw: *mut sqlite3_file, out: *mut c_int) -> c_int {
    // A connection holding SHARED has the exclusive OS lock, so another
    // connection cannot hold RESERVED. This is also correct for hot journals.
    *out = i32::from(file(raw).lock >= SQLITE_LOCK_RESERVED);
    SQLITE_OK
}
unsafe extern "C" fn control(_: *mut sqlite3_file, _: c_int, _: *mut c_void) -> c_int {
    SQLITE_NOTFOUND
}
unsafe extern "C" fn sector(_: *mut sqlite3_file) -> c_int {
    4096
}
unsafe extern "C" fn characteristics(_: *mut sqlite3_file) -> c_int {
    0
}
static METHODS: sqlite3_io_methods = sqlite3_io_methods {
    iVersion: 1,
    xClose: Some(close),
    xRead: Some(read),
    xWrite: Some(write),
    xTruncate: Some(truncate),
    xSync: Some(sync),
    xFileSize: Some(size),
    xLock: Some(lock),
    xUnlock: Some(unlock),
    xCheckReservedLock: Some(reserved),
    xFileControl: Some(control),
    xSectorSize: Some(sector),
    xDeviceCharacteristics: Some(characteristics),
    xShmMap: None,
    xShmLock: None,
    xShmBarrier: None,
    xShmUnmap: None,
    xFetch: None,
    xUnfetch: None,
};
