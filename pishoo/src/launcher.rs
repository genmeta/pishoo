use std::{ffi::CString, os::fd::OwnedFd, path::Path};

use nix::unistd::{
    execve, fork, getegid, geteuid, getgid, getgrouplist, getuid, pipe, setgid, setgroups, setuid,
    sysconf, ForkResult, Gid, SysconfVar, Uid,
};
use tokio::fs::File;

use crate::worker_spawn::WorkerHandle;

pub struct WorkerTransport {
    pub stdin: File,
    pub stdout: File,
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
    home: &Path,
) -> Result<LaunchedWorker, std::io::Error> {
    let supplementary_groups = resolve_supplementary_groups(username, gid)?;
    let worker_bin = CString::new(worker_bin.as_os_str().as_encoded_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "worker path contains NUL byte",
        )
    })?;
    let env = build_exec_env(username, home)?;
    let argv = vec![worker_bin.clone()];
    let max_fd = max_fd();

    let (child_stdin_read, parent_stdin_write) = pipe_pair()?;
    let (parent_stdout_read, child_stdout_write) = pipe_pair()?;

    // SAFETY: fork semantics require unsafe; child path immediately performs exec/exit only.
    match unsafe { fork() }.map_err(std::io::Error::from)? {
        ForkResult::Child => {
            child_exec(
                &worker_bin,
                &argv,
                &env,
                uid,
                gid,
                &supplementary_groups,
                &child_stdin_read,
                &child_stdout_write,
                max_fd,
            );
        }
        ForkResult::Parent { child } => {
            drop(child_stdin_read);
            drop(child_stdout_write);
            let stdin = File::from_std(std::fs::File::from(parent_stdin_write));
            let stdout = File::from_std(std::fs::File::from(parent_stdout_read));

            Ok(LaunchedWorker {
                handle: WorkerHandle::from_unix_pid(child.as_raw() as u32),
                transport: WorkerTransport { stdin, stdout },
            })
        }
    }
}

fn child_exec(
    worker_bin: &CString,
    argv: &[CString],
    envp: &[CString],
    uid: Uid,
    gid: u32,
    supplementary_groups: &[Gid],
    stdin_fd: &OwnedFd,
    stdout_fd: &OwnedFd,
    max_fd: i32,
) -> ! {
    if nix::unistd::dup2_stdin(stdin_fd).is_err() {
        child_fail(126);
    }
    if nix::unistd::dup2_stdout(stdout_fd).is_err() {
        child_fail(126);
    }

    let mut fd = 3;
    while fd < max_fd {
        let _ = nix::unistd::close(fd);
        fd += 1;
    }

    let current_uid = getuid();
    let current_euid = geteuid();
    let current_gid = getgid();
    let current_egid = getegid();
    let gid = Gid::from_raw(gid);

    if current_euid.is_root() {
        if setgroups(supplementary_groups).is_err() {
            child_fail(126);
        }
        if setgid(gid).is_err() {
            child_fail(126);
        }
        if setuid(uid).is_err() {
            child_fail(126);
        }
        if getuid() != uid || geteuid() != uid || getgid() != gid || getegid() != gid {
            child_fail(126);
        }
    } else if current_uid != uid || current_euid != uid || current_gid != gid || current_egid != gid
    {
        child_fail(126);
    }

    let _ = execve(worker_bin, argv, envp);
    child_fail(127);
}

fn pipe_pair() -> Result<(OwnedFd, OwnedFd), std::io::Error> {
    pipe().map_err(std::io::Error::from)
}

fn build_exec_env(username: &str, home: &Path) -> Result<Vec<CString>, std::io::Error> {
    let path = std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into());
    [
        [b"HOME=".as_slice(), home.as_os_str().as_encoded_bytes()].concat(),
        [b"USER=".as_slice(), username.as_bytes()].concat(),
        [b"LOGNAME=".as_slice(), username.as_bytes()].concat(),
        [b"PATH=".as_slice(), path.as_os_str().as_encoded_bytes()].concat(),
    ]
    .into_iter()
    .map(|entry| {
        CString::new(entry).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "environment contains NUL byte",
            )
        })
    })
    .collect()
}

fn child_fail(code: i32) -> ! {
    std::process::exit(code);
}

fn max_fd() -> i32 {
    match sysconf(SysconfVar::OPEN_MAX) {
        Ok(Some(open_max)) if open_max > 0 => open_max as i32,
        _ => 1024,
    }
}

fn resolve_supplementary_groups(username: &str, gid: u32) -> Result<Vec<Gid>, std::io::Error> {
    let username = CString::new(username).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "username contains NUL byte",
        )
    })?;
    getgrouplist(&username, Gid::from_raw(gid)).map_err(std::io::Error::from)
}
