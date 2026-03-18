use std::{ffi::CString, path::Path};

use nix::unistd::{Gid, Uid};
use tokio::process::{ChildStdin, ChildStdout};

use crate::worker_spawn::WorkerHandle;

pub struct WorkerTransport {
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
}

pub struct LaunchedWorker {
    pub handle: WorkerHandle,
    pub transport: WorkerTransport,
}

pub fn launch_worker(
    worker_bin: &Path,
    uid: Uid,
    gid: u32,
    username: &str,
) -> Result<LaunchedWorker, std::io::Error> {
    let supplementary_groups = resolve_supplementary_groups(username, gid)?;

    let mut command = tokio::process::Command::new(worker_bin);
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit());

    unsafe {
        let groups = supplementary_groups;
        let nix_gid = Gid::from_raw(gid);
        let nix_uid = uid;
        command.pre_exec(move || {
            if libc::setgroups(groups.len(), groups.as_ptr()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            nix::unistd::setgid(nix_gid)?;
            nix::unistd::setuid(nix_uid)?;
            if libc::getuid() != nix_uid.as_raw()
                || libc::geteuid() != nix_uid.as_raw()
                || libc::getgid() != nix_gid.as_raw()
                || libc::getegid() != nix_gid.as_raw()
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "credential verification failed after setuid/setgid",
                ));
            }
            Ok(())
        });
    }

    let mut child = command.spawn()?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("failed to capture child stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("failed to capture child stdout"))?;

    Ok(LaunchedWorker {
        handle: WorkerHandle::new(child),
        transport: WorkerTransport { stdin, stdout },
    })
}

fn resolve_supplementary_groups(
    username: &str,
    gid: u32,
) -> Result<Vec<libc::gid_t>, std::io::Error> {
    let username = CString::new(username).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "username contains NUL byte",
        )
    })?;
    let mut ngroups: libc::c_int = 0;
    let _ = unsafe {
        libc::getgrouplist(
            username.as_ptr(),
            gid as libc::gid_t,
            std::ptr::null_mut(),
            &mut ngroups,
        )
    };
    if ngroups <= 0 {
        return Ok(vec![gid as libc::gid_t]);
    }
    let mut groups = vec![0 as libc::gid_t; ngroups as usize];
    let ret = unsafe {
        libc::getgrouplist(
            username.as_ptr(),
            gid as libc::gid_t,
            groups.as_mut_ptr(),
            &mut ngroups,
        )
    };
    if ret == -1 {
        return Err(std::io::Error::other(
            "failed to resolve supplementary groups",
        ));
    }
    groups.truncate(ngroups as usize);
    Ok(groups)
}
